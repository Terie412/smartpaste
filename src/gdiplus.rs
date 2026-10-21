//! GDI+ 的最小封装，只用来画抗锯齿的圆角和文字。
//!
//! 为什么需要它：GDI 的 `RoundRect` / `Ellipse` / 斜线全是硬边像素，
//! 8px 圆角在 1:1 显示下能清楚看到台阶。GDI+ 打开 `SmoothingModeAntiAlias`
//! 之后边缘是混合过的，视觉上立刻平滑。
//!
//! windows-sys 0.59 只提供了 GDI+ 的类型定义、没有函数绑定，
//! 所以这里自己声明 extern —— GDI+ 是扁平的 C API，符号名没有修饰。

use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::HDC;

#[repr(C)]
struct StartupInput {
    version: u32,
    callback: *const core::ffi::c_void,
    no_thread_lock: i32,
    no_codecs: i32,
}

#[link(name = "gdiplus")]
extern "system" {
    fn GdiplusStartup(
        token: *mut usize,
        input: *const StartupInput,
        output: *mut core::ffi::c_void,
    ) -> i32;

    fn GdipCreateFromHDC(hdc: HDC, graphics: *mut *mut core::ffi::c_void) -> i32;
    fn GdipDeleteGraphics(g: *mut core::ffi::c_void) -> i32;
    fn GdipSetSmoothingMode(g: *mut core::ffi::c_void, mode: i32) -> i32;

    fn GdipCreateSolidFill(color: u32, brush: *mut *mut core::ffi::c_void) -> i32;
    fn GdipDeleteBrush(b: *mut core::ffi::c_void) -> i32;
    fn GdipCreatePen1(
        color: u32,
        width: f32,
        unit: i32,
        pen: *mut *mut core::ffi::c_void,
    ) -> i32;
    fn GdipDeletePen(p: *mut core::ffi::c_void) -> i32;

    fn GdipCreatePath(mode: i32, path: *mut *mut core::ffi::c_void) -> i32;
    fn GdipDeletePath(p: *mut core::ffi::c_void) -> i32;
    fn GdipAddPathArc(
        path: *mut core::ffi::c_void,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        start: f32,
        sweep: f32,
    ) -> i32;
    fn GdipAddPathLine(
        path: *mut core::ffi::c_void,
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
    ) -> i32;
    fn GdipClosePathFigure(path: *mut core::ffi::c_void) -> i32;
    fn GdipFillPath(g: *mut core::ffi::c_void, brush: *mut core::ffi::c_void, path: *mut core::ffi::c_void)
        -> i32;
    fn GdipDrawPath(g: *mut core::ffi::c_void, pen: *mut core::ffi::c_void, path: *mut core::ffi::c_void)
        -> i32;

    fn GdipDrawLine(
        g: *mut core::ffi::c_void,
        pen: *mut core::ffi::c_void,
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
    ) -> i32;
    fn GdipDrawEllipse(
        g: *mut core::ffi::c_void,
        pen: *mut core::ffi::c_void,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> i32;
    fn GdipDrawRectangle(
        g: *mut core::ffi::c_void,
        pen: *mut core::ffi::c_void,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> i32;
    fn GdipFillRectangle(
        g: *mut core::ffi::c_void,
        brush: *mut core::ffi::c_void,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> i32;
}

const SMOOTH_AA: i32 = 4; // SmoothingModeAntiAlias
const UNIT_PIXEL: i32 = 2; // UnitPixel
const FILL_WINDING: i32 = 1; // FillModeWinding

/// 进程启动时调一次。失败也不致命，只是绘制退回硬边。
pub fn startup() {
    unsafe {
        // SuppressBackgroundThread 必须是 FALSE：置 1 时 GDI+ 要求同时传出一个
        // GdiplusStartupOutput，传 NULL 会直接返回 GenericError(2)。
        let input = StartupInput {
            version: 1,
            callback: core::ptr::null(),
            no_thread_lock: 0,
            no_codecs: 1,
        };
        let mut token = 0usize;
        let st = GdiplusStartup(&mut token, &input, core::ptr::null_mut());
        if st != 0 {
            crate::win::trace(&format!("gdiplus: startup failed {st}"));
        }
    }
}

/// GDI 的 COLORREF 是 0x00BBGGRR，GDI+ 的 ARGB 是 0xAARRGGBB
fn argb(colorref: u32) -> u32 {
    let r = colorref & 0xFF;
    let g = (colorref >> 8) & 0xFF;
    let b = (colorref >> 16) & 0xFF;
    0xFF00_0000 | (r << 16) | (g << 8) | b
}

unsafe fn with_graphics<F: FnOnce(*mut core::ffi::c_void)>(dc: HDC, f: F) {
    let mut g: *mut core::ffi::c_void = core::ptr::null_mut();
    if GdipCreateFromHDC(dc, &mut g) != 0 || g.is_null() {
        return;
    }
    GdipSetSmoothingMode(g, SMOOTH_AA);
    f(g);
    GdipDeleteGraphics(g);
}

/// 圆角矩形路径，半径是像素
unsafe fn round_path(r: &RECT, radius: f32) -> *mut core::ffi::c_void {
    let mut path: *mut core::ffi::c_void = core::ptr::null_mut();
    if GdipCreatePath(FILL_WINDING, &mut path) != 0 {
        return core::ptr::null_mut();
    }
    let (x, y) = (r.left as f32, r.top as f32);
    let (w, h) = ((r.right - r.left) as f32, (r.bottom - r.top) as f32);
    let d = (radius * 2.0).min(w).min(h);

    GdipAddPathArc(path, x, y, d, d, 180.0, 90.0);
    GdipAddPathLine(path, x + radius, y, x + w - radius, y);
    GdipAddPathArc(path, x + w - d, y, d, d, 270.0, 90.0);
    GdipAddPathLine(path, x + w, y + radius, x + w, y + h - radius);
    GdipAddPathArc(path, x + w - d, y + h - d, d, d, 0.0, 90.0);
    GdipAddPathLine(path, x + w - radius, y + h, x + radius, y + h);
    GdipAddPathArc(path, x, y + h - d, d, d, 90.0, 90.0);
    GdipClosePathFigure(path);
    path
}

/// 抗锯齿填充圆角矩形
pub unsafe fn fill_round(dc: HDC, r: &RECT, radius: f32, colorref: u32) {
    let path = round_path(r, radius);
    if path.is_null() {
        crate::win::trace("gdi: path null");
        return;
    }
    let mut brush: *mut core::ffi::c_void = core::ptr::null_mut();
    let sb = GdipCreateSolidFill(argb(colorref), &mut brush);
    if sb != 0 {
        crate::win::trace(&format!("gdi: brush fail {sb}"));
        GdipDeletePath(path);
        return;
    }
    let mut g: *mut core::ffi::c_void = core::ptr::null_mut();
    let sg = GdipCreateFromHDC(dc, &mut g);
    if sg != 0 || g.is_null() {
        crate::win::trace(&format!("gdi: graphics fail {sg}"));
        GdipDeleteBrush(brush);
        GdipDeletePath(path);
        return;
    }
    GdipSetSmoothingMode(g, SMOOTH_AA);
    let sf = GdipFillPath(g, brush, path);
    if sf != 0 {
        crate::win::trace(&format!("gdi: fill fail {sf}"));
    }
    GdipDeleteGraphics(g);
    GdipDeleteBrush(brush);
    GdipDeletePath(path);
}

/// 抗锯齿描边圆角矩形
pub unsafe fn stroke_round(dc: HDC, r: &RECT, radius: f32, colorref: u32, width: f32) {
    let path = round_path(r, radius);
    if path.is_null() {
        return;
    }
    let mut pen: *mut core::ffi::c_void = core::ptr::null_mut();
    if GdipCreatePen1(argb(colorref), width, UNIT_PIXEL, &mut pen) != 0 {
        GdipDeletePath(path);
        return;
    }
    with_graphics(dc, |g| {
        GdipDrawPath(g, pen, path);
    });
    GdipDeletePen(pen);
    GdipDeletePath(path);
}

pub unsafe fn line(dc: HDC, x1: i32, y1: i32, x2: i32, y2: i32, colorref: u32, width: f32) {
    let mut pen: *mut core::ffi::c_void = core::ptr::null_mut();
    if GdipCreatePen1(argb(colorref), width, UNIT_PIXEL, &mut pen) != 0 {
        return;
    }
    with_graphics(dc, |g| {
        GdipDrawLine(g, pen, x1 as f32, y1 as f32, x2 as f32, y2 as f32);
    });
    GdipDeletePen(pen);
}

pub unsafe fn ellipse(dc: HDC, r: &RECT, colorref: u32, width: f32) {
    let mut pen: *mut core::ffi::c_void = core::ptr::null_mut();
    if GdipCreatePen1(argb(colorref), width, UNIT_PIXEL, &mut pen) != 0 {
        return;
    }
    with_graphics(dc, |g| {
        GdipDrawEllipse(
            g,
            pen,
            r.left as f32,
            r.top as f32,
            (r.right - r.left) as f32,
            (r.bottom - r.top) as f32,
        );
    });
    GdipDeletePen(pen);
}

pub unsafe fn rect_stroke(dc: HDC, r: &RECT, colorref: u32, width: f32) {
    let mut pen: *mut core::ffi::c_void = core::ptr::null_mut();
    if GdipCreatePen1(argb(colorref), width, UNIT_PIXEL, &mut pen) != 0 {
        return;
    }
    with_graphics(dc, |g| {
        GdipDrawRectangle(
            g,
            pen,
            r.left as f32,
            r.top as f32,
            (r.right - r.left) as f32,
            (r.bottom - r.top) as f32,
        );
    });
    GdipDeletePen(pen);
}

pub unsafe fn rect_fill(dc: HDC, r: &RECT, colorref: u32) {
    let mut brush: *mut core::ffi::c_void = core::ptr::null_mut();
    if GdipCreateSolidFill(argb(colorref), &mut brush) != 0 {
        return;
    }
    with_graphics(dc, |g| {
        GdipFillRectangle(
            g,
            brush,
            r.left as f32,
            r.top as f32,
            (r.right - r.left) as f32,
            (r.bottom - r.top) as f32,
        );
    });
    GdipDeleteBrush(brush);
}
