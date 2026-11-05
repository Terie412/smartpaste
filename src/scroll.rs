//! 滚动截图会话：选区外的细框 + 可点的控制条，定时抓选区、交给 `stitch` 拼。
//!
//! 为什么不是"点一下就自动滚"：滚轮注入在不同程序里行为差得太远（有的按行、
//! 有的按像素、有的带惯性动画），而且很多窗口根本不吃合成滚轮。所以第一版
//! 让用户自己滚，程序只负责"抓稳 → 对齐 → 拼"，容错高得多。
//!
//! 两个辅助窗口都不在选区里，也不会被截进去：
//! - 细框：3px，紧贴选区外侧，点击穿透
//! - 控制条：优先放选区上方，放不下挪到下方
//!
//! 抓帧在主线程用定时器做（120ms 一次），不另开线程：一次抓 900x560 的位图
//! 加算行指纹是几毫秒的事，而跨线程传图反而要复制一大堆像素。

use crate::stitch::{self, Frame, Push, Stitcher};
use crate::win::{hinstance, rect, to_wide};
use image::RgbaImage;
use windows_sys::Win32::Foundation::{HWND, POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateDIBSection,
    CreateRoundRectRgn, DeleteDC, DeleteObject, EndPaint, GetDC, GetDIBits, GetMonitorInfoW,
    MonitorFromRect, ReleaseDC, SelectObject, SetWindowRgn, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    PAINTSTRUCT, SRCCOPY, AC_SRC_ALPHA, AC_SRC_OVER,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetSystemMetrics, KillTimer, LoadCursorW,
    PostMessageW, RegisterClassW, SetCursor, SetTimer, SetWindowDisplayAffinity, ShowWindow,
    UpdateLayeredWindow, IDC_HAND, SM_CXSCREEN, SM_CYSCREEN, SW_SHOWNOACTIVATE, ULW_ALPHA,
    WDA_EXCLUDEFROMCAPTURE, WM_APP, WM_LBUTTONUP,
    WM_PAINT, WM_SETCURSOR, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

const FRAME_CLASS: &str = "SmartPasteScrollFrame";
const HUD_CLASS: &str = "SmartPasteScrollHud";

/// 细框贴边厚度，整圈在选区外面
const BORDER: i32 = 3;
/// 框的颜色。写字时是 0xAARRGGBB，所以高字节是 A、低三个字节按 BGR 排。
const FRAME_COLOR: u32 = 0x00FF_2E63; // #FF2E63

const HUD_W: i32 = 420;
const HUD_H: i32 = 48;
/// 控制条上三个可点区域的左边界：复制 / 保存 / 取消
const BTN_COPY_X: i32 = 208;
const BTN_SAVE_X: i32 = 280;
const BTN_CANCEL_X: i32 = 352;

/// 抓帧间隔。120ms 约 8Hz：够密，不会漏掉用户一屏一屏的滚动；
/// 又不至于每帧都去 BitBlt 抢 DWM 的合成结果。
const POLL_MS: u32 = 120;
/// 滚过之后，多久没再滚就认为收工了。
///
/// 别设太短：手动滚的人会停下来看内容，5 秒就收工等于打断阅读。
/// 8 秒是"停手了"和"在看"之间比较稳的分界。
const IDLE_MS: u64 = 8000;
/// 累计增长到多少才算"真的滚了"。桌面噪声能蹭出几像素，这里给一屏的十几分之一。
const SCROLLED_MIN: u32 = 60;
/// **一次都没滚过**时的兜底：这时候不能按 IDLE 收工，用户很可能还在
/// 摆窗口/找位置。给足 90 秒，只有彻底忘了这回事才自动关。
const STUCK_MS: u64 = 90_000;
/// 长图最大高度（像素）。再高就不是"长截图"而是录屏了。
const MAX_H: u32 = 20_000;
/// 画布内存上限：宽 2560 时 20000 行是 200MB，兜一下
const MAX_BYTES: u64 = 256 << 20;

/// 主窗口收到它去取图（wParam 是动作）
pub const WM_SCROLL_ACTION: u32 = WM_APP + 8;
/// 控制条上的三个动作
pub const ACT_COPY: usize = 1;
pub const ACT_SAVE: usize = 2;
pub const ACT_CANCEL: usize = 3;

struct Session {
    main: HWND,
    rect: RECT,
    frame_win: HWND,
    hud: HWND,
    st: Stitcher,
    /// 最近一次抓到、还没确认稳定的帧
    pending: Option<Frame>,
    /// 最近一次"拼进新内容"的时刻（毫秒）
    last_change: u64,
    /// 会话开始的时刻（毫秒），用来判断"压根没滚过"
    started: u64,
    /// 成功拼进去的段数（不含第一帧的全量铺底）
    appends: u32,
    /// 累计拼进去的行数。用它而不是段数判断"用户到底滚没滚"：
    /// 桌面本身会动（动画、光标、聊天窗口），偶尔会蹭出几像素的假位移，
    /// 这种噪声不能算"滚过"。
    grown: u32,
    /// 已经在等主窗口收尾，别再重复发消息
    closing: bool,
}

static mut S: Option<Session> = None;

pub fn register() {
    unsafe {
        for (class, proc) in [(FRAME_CLASS, frame_proc as WNDPROC), (HUD_CLASS, hud_proc as WNDPROC)]
        {
            let wide = to_wide(class);
            let wc = WNDCLASSW {
                lpfnWndProc: Some(proc),
                hInstance: hinstance(),
                hCursor: LoadCursorW(std::ptr::null_mut(), IDC_HAND),
                lpszClassName: wide.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassW(&wc);
        }
    }
}

type WNDPROC = unsafe extern "system" fn(HWND, u32, usize, isize) -> isize;

pub fn start(main: HWND, rect: RECT) -> bool {
    unsafe {
        if S.is_some() {
            return false;
        }
        let w = rect.right - rect.left;
        let h = rect.bottom - rect.top;
        if w < 16 || h < 16 {
            crate::win::trace("scroll: 选区太小");
            return false;
        }

        let Some(frame_win) = create_frame(&rect) else {
            crate::win::trace("scroll: 细框创建失败");
            return false;
        };
        let Some(hud) = create_hud(&rect, frame_win) else {
            DestroyWindow(frame_win);
            crate::win::trace("scroll: 控制条创建失败");
            return false;
        };

        let cap = ((MAX_BYTES / (w as u64 * 4)).max(200) as u32).min(MAX_H);
        let st = Stitcher::new(w as u32, h as u32, cap);

        S = Some(Session {
            main,
            rect,
            frame_win,
            hud,
            st,
            pending: None,
            last_change: now(),
            started: now(),
            appends: 0,
            grown: 0,
            closing: false,
        });

        SetTimer(frame_win, 1, POLL_MS, None);
        paint_hud(hud, 0, 0);
        crate::win::trace(&format!(
            "scroll: 开始 {}x{} at ({},{}) 上限 {}",
            w, h, rect.left, rect.top, cap
        ));
        true
    }
}

/// 收工结果。`appends == 0` 说明用户一次都没滚过，交出去的就是首屏。
pub struct Capture {
    pub img: RgbaImage,
    pub appends: u32,
}

/// 收工：拆窗口、把拼好的图交出去。没有内容就返回 None。
pub fn finish() -> Option<Capture> {
    unsafe {
        let s = S.take()?;
        teardown(&s);
        crate::win::trace(&format!(
            "scroll: 收工 {} 帧 {}px",
            s.st.frames,
            s.st.height()
        ));
        let appends = s.appends;
        s.st.finish().map(|img| Capture { img, appends })
    }
}

pub fn cancel() {
    unsafe {
        if let Some(s) = S.take() {
            teardown(&s);
            crate::win::trace("scroll: 取消");
        }
    }
}

unsafe fn teardown(s: &Session) {
    KillTimer(s.frame_win, 1);
    if !s.hud.is_null() {
        DestroyWindow(s.hud);
    }
    if !s.frame_win.is_null() {
        DestroyWindow(s.frame_win);
    }
}

fn now() -> u64 {
    unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() }
}

// ---------------------------------------------------------------- 抓帧

/// 抓选区的一块。用 CAPTUREBLT 把分层窗口（提示框、半透明面板）也算进来，
/// 否则截图里会缺一块。
unsafe fn grab(rect: &RECT) -> Option<RgbaImage> {
    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    if w <= 0 || h <= 0 {
        return None;
    }
    let screen = GetDC(std::ptr::null_mut());
    if screen.is_null() {
        return None;
    }
    let mem = CreateCompatibleDC(screen);
    let bmp = CreateCompatibleBitmap(screen, w, h);
    if mem.is_null() || bmp.is_null() {
        DeleteDC(mem);
        DeleteObject(bmp as HGDIOBJ);
        ReleaseDC(std::ptr::null_mut(), screen);
        return None;
    }
    let old = SelectObject(mem, bmp);
    // 0x4000_0000 = CAPTUREBLT
    let ok = BitBlt(mem, 0, 0, w, h, screen, rect.left, rect.top, SRCCOPY | 0x4000_0000);

    let mut bi: BITMAPINFO = std::mem::zeroed();
    bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = w;
    bi.bmiHeader.biHeight = -h; // 负高度 = 自上而下
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB;

    let mut buf = vec![0u8; (w * h * 4) as usize];
    let lines = GetDIBits(
        mem,
        bmp,
        0,
        h as u32,
        buf.as_mut_ptr() as *mut core::ffi::c_void,
        &mut bi,
        DIB_RGB_COLORS,
    );

    SelectObject(mem, old);
    DeleteObject(bmp as HGDIOBJ);
    DeleteDC(mem);
    ReleaseDC(std::ptr::null_mut(), screen);

    if ok == 0 || lines == 0 {
        return None;
    }
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2); // BGRA -> RGBA
        px[3] = 255;
    }
    RgbaImage::from_raw(w as u32, h as u32, buf)
}

// ---------------------------------------------------------------- 定时抓帧

unsafe fn on_timer() {
    let Some(s) = S.as_mut() else { return };
    if s.closing {
        return;
    }
    let Some(img) = grab(&s.rect) else { return };
    let frame = Frame::new(img);

    match s.pending.take() {
        // 第一次抓到，等下一次比较
        None => {
            s.pending = Some(frame);
        }
        // 连续两次抓到一样的东西 ⇒ 画面稳住了，这一帧才可以拿去拼。
        // 直接抓一帧就拼是不行的：滚动惯性中间那些帧会拼出重影。
        Some(prev) => {
            if stitch::similar(&prev, &frame) {
                match s.st.push(frame) {
                    Push::Appended(bytes) => {
                        s.last_change = now();
                        s.appends += 1;
                        s.grown = s.grown.saturating_add(bytes);
                        paint_hud(s.hud, s.st.height(), s.st.frames);
                        crate::win::trace(&format!("scroll: +{bytes}px -> {}", s.st.height()));
                    }
                    Push::Full => {
                        crate::win::trace("scroll: 到高度上限");
                        req_close(s, ACT_COPY);
                    }
                    Push::Same | Push::Rejected => {}
                }
            } else {
                s.pending = Some(frame);
            }
        }
    }

    // 收工判定分两种：
    //   滚过之后停手 8 秒 → 确实滚完了，直接复制
    //   一次都没滚过      → 用户可能还在摆窗口，只有 90 秒没动静才关，
    //                       而且关了也不复制（首屏就是普通截图，没意义）
    if s.closing {
        return;
    }
    let idle = now().saturating_sub(s.last_change);
    let scrolled = s.grown >= SCROLLED_MIN;
    if scrolled && idle > IDLE_MS {
        crate::win::trace("scroll: 滚完停手，自动收工");
        req_close(s, ACT_COPY);
    } else if !scrolled && now().saturating_sub(s.started) > STUCK_MS {
        crate::win::trace("scroll: 一直没滚，自动关闭");
        // 走"收工"而不是"取消"：主窗口会把 appends == 0 翻译成一句
        // "没有检测到滚动"，比默默关掉强
        req_close(s, ACT_COPY);
    }
}

/// 让主窗口来做收尾（复制/保存/取消），这里只发消息。
unsafe fn req_close(s: &mut Session, action: usize) {
    if s.closing {
        return;
    }
    s.closing = true;
    KillTimer(s.frame_win, 1);
    PostMessageW(s.main, WM_SCROLL_ACTION, action, 0);
}

// ---------------------------------------------------------------- 窗口

unsafe fn create_frame(rect: &RECT) -> Option<HWND> {
    let w = rect.right - rect.left + BORDER * 2;
    let h = rect.bottom - rect.top + BORDER * 2;
    let (x, y) = (rect.left - BORDER, rect.top - BORDER);

    let class = to_wide(FRAME_CLASS);
    let title = to_wide("SmartPaste 滚动截图");
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
        class.as_ptr(),
        title.as_ptr(),
        WS_POPUP,
        x,
        y,
        w,
        h,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        hinstance(),
        std::ptr::null(),
    );
    if hwnd.is_null() {
        return None;
    }

    let screen = GetDC(std::ptr::null_mut());
    if screen.is_null() {
        DestroyWindow(hwnd);
        return None;
    }
    let dc = CreateCompatibleDC(screen);
    let mut bi: BITMAPINFO = std::mem::zeroed();
    bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = w;
    bi.bmiHeader.biHeight = -h;
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB;

    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let bmp: HBITMAP = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
    if bmp.is_null() || bits.is_null() {
        DeleteDC(dc);
        ReleaseDC(std::ptr::null_mut(), screen);
        DestroyWindow(hwnd);
        return None;
    }
    let old = SelectObject(dc, bmp as HGDIOBJ);

    // 整圈画在选区外面，中间留透明
    let px = std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);
    px.fill(0);
    for row in 0..h {
        for col in 0..w {
            let edge = col < BORDER || row < BORDER || col >= w - BORDER || row >= h - BORDER;
            if edge {
                px[(row * w + col) as usize] = 0xFF00_0000 | FRAME_COLOR;
            }
        }
    }

    blit_layered(hwnd, screen, dc, x, y, w, h);

    SelectObject(dc, old);
    DeleteObject(bmp as HGDIOBJ);
    DeleteDC(dc);
    ReleaseDC(std::ptr::null_mut(), screen);

    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    Some(hwnd)
}

unsafe fn blit_layered(hwnd: HWND, screen: HDC, dc: HDC, x: i32, y: i32, w: i32, h: i32) {
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let dst = POINT { x, y };
    let size = SIZE { cx: w, cy: h };
    let src = POINT { x: 0, y: 0 };
    UpdateLayeredWindow(hwnd, screen, &dst, &size, dc, &src, 0, &blend, ULW_ALPHA);
}

/// 控制条位置：优先选区上方，其次下方，都放不下就压在选区内部右下角。
///
/// 位置必须夹在**显示器工作区**里。早先按"屏幕高度"算，全屏选区时控制条会落到
/// 任务栏底下——窗口存在、IsWindowVisible 也是真，但用户根本看不见，
/// 反馈里就成了"没有控制条"。
fn hud_pos(rect: &RECT) -> (i32, i32, bool) {
    let gap = 8;
    let work = work_area(rect);
    let max_x = (work.right - HUD_W).max(work.left);
    let x = rect.left.clamp(work.left, max_x);

    let above = rect.top - HUD_H - gap;
    if above >= work.top {
        return (x, above, true);
    }
    let below = rect.bottom + gap;
    if below + HUD_H <= work.bottom {
        return (x, below, true);
    }
    // 四面都顶死（全屏选区）：压在自己右下角。它会盖住一点内容，
    // 但 `create_hud` 会给它设 WDA_EXCLUDEFROMCAPTURE，不会截进图里。
    // 全屏选区时 rect.bottom 就是屏幕底边，还得再夹一次工作区底边，
    // 否则算出来仍然压在任务栏下面
    let inside_y = (rect.bottom - HUD_H - gap)
        .min(work.bottom - HUD_H)
        .max(work.top);
    let inside_x = (rect.right - HUD_W - gap).max(work.left);
    (inside_x.min(max_x), inside_y, false)
}

/// 选区所在显示器的工作区（已扣掉任务栏）
fn work_area(sel: &RECT) -> RECT {
    unsafe {
        let hmon = MonitorFromRect(sel, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(hmon, &mut mi) != 0 {
            return mi.rcWork;
        }
        rect(0, 0, GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN))
    }
}

unsafe fn create_hud(rect: &RECT, owner: HWND) -> Option<HWND> {
    let (x, y, outside) = hud_pos(rect);
    let class = to_wide(HUD_CLASS);
    let title = to_wide("SmartPaste 滚动截图控制");
    // 和录制的控制条一样不用 layered：SetWindowDisplayAffinity 和
    // UpdateLayeredWindow 一起用会让窗口在屏幕上直接消失。普通窗口 + 圆角区域最稳。
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
        class.as_ptr(),
        title.as_ptr(),
        WS_POPUP,
        x,
        y,
        HUD_W,
        HUD_H,
        owner,
        std::ptr::null_mut(),
        hinstance(),
        std::ptr::null(),
    );
    if hwnd.is_null() {
        return None;
    }

    let rgn = CreateRoundRectRgn(0, 0, HUD_W + 1, HUD_H + 1, 20, 20);
    SetWindowRgn(hwnd, rgn, 0);

    ShowWindow(hwnd, SW_SHOWNOACTIVATE);

    if !outside {
        // 实在没地方放，那就只能压在选区边上，得把它从截图里排除掉
        SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
    }
    Some(hwnd)
}

// ---------------------------------------------------------------- 控制条绘制

/// 进度文案。0 行的时候提示用户开始滚。
unsafe fn paint_hud(hwnd: HWND, height: u32, frames: u32) {
    if hwnd.is_null() {
        return;
    }
    let hdc = GetDC(hwnd);
    if hdc.is_null() {
        return;
    }
    paint_hud_dc(hdc, height, frames);
    ReleaseDC(hwnd, hdc);
}

/// 画到任意 DC 上。WM_PAINT 里用 BeginPaint 的 DC，平时用 GetDC 的 DC，
/// 两边共用一份绘制代码。
unsafe fn paint_hud_dc(hdc: HDC, height: u32, frames: u32) {
    let screen = GetDC(std::ptr::null_mut());
    let dc = CreateCompatibleDC(screen);
    let mut bi: BITMAPINFO = std::mem::zeroed();
    bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = HUD_W;
    bi.bmiHeader.biHeight = -HUD_H;
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB;

    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let bmp: HBITMAP = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
    if bmp.is_null() || bits.is_null() {
        DeleteDC(dc);
        ReleaseDC(std::ptr::null_mut(), screen);
        return;
    }
    let old = SelectObject(dc, bmp as HGDIOBJ);
    std::slice::from_raw_parts_mut(bits as *mut u32, (HUD_W * HUD_H) as usize).fill(0);

    let panel = rect(0, 0, HUD_W, HUD_H);
    crate::gdiplus::fill_round(dc, &panel, 10.0, rgb(0x14, 0x14, 0x18));

    // 两行：第一行说这是什么，第二行说现在怎么样了 / 下一步该干嘛
    let f_title = crate::gdiplus::FontSpec::new(12.5, true, "Microsoft YaHei");
    crate::gdiplus::draw_text(dc, &f_title, rgb(0xF2, 0xF0, 0xF4), 16, 7, "滚动截图");
    let f_sub = crate::gdiplus::FontSpec::new(10.5, false, "Microsoft YaHei");
    let sub = if frames <= 1 {
        "向下滚轮继续，拼完点复制".to_string()
    } else {
        format!("已拼 {height} px · {frames} 帧")
    };
    crate::gdiplus::draw_text(dc, &f_sub, rgb(0x9A, 0x9A, 0xA4), 16, 28, &sub);

    let f_btn = crate::gdiplus::FontSpec::new(12.0, true, "Microsoft YaHei");
    // 复制：主操作，实心玫红
    let b = rect(BTN_COPY_X, 8, BTN_SAVE_X - 6, HUD_H - 8);
    crate::gdiplus::fill_round(dc, &b, 7.0, rgb(0xFF, 0x2E, 0x63));
    let (tw, th) = crate::gdiplus::measure_text(dc, &f_btn, "复制");
    crate::gdiplus::draw_text(
        dc,
        &f_btn,
        rgb(0xFF, 0xFF, 0xFF),
        (b.left + b.right - tw) / 2,
        (b.top + b.bottom - th) / 2,
        "复制",
    );
    // 保存：次要，描边样式
    let b = rect(BTN_SAVE_X, 8, BTN_CANCEL_X - 6, HUD_H - 8);
    crate::gdiplus::stroke_round(dc, &b, 7.0, rgb(0x3E, 0x3E, 0x48), 1.4);
    let (tw, th) = crate::gdiplus::measure_text(dc, &f_btn, "保存");
    crate::gdiplus::draw_text(
        dc,
        &f_btn,
        rgb(0xD8, 0xD8, 0xE0),
        (b.left + b.right - tw) / 2,
        (b.top + b.bottom - th) / 2,
        "保存",
    );
    // 取消：纯文字
    let (tw, th) = crate::gdiplus::measure_text(dc, &f_btn, "取消");
    crate::gdiplus::draw_text(
        dc,
        &f_btn,
        rgb(0x9A, 0x9A, 0xA4),
        (BTN_CANCEL_X + HUD_W - tw) / 2,
        (HUD_H - th) / 2,
        "取消",
    );

    BitBlt(hdc, 0, 0, HUD_W, HUD_H, dc, 0, 0, SRCCOPY);

    SelectObject(dc, old);
    DeleteObject(bmp as HGDIOBJ);
    DeleteDC(dc);
    ReleaseDC(std::ptr::null_mut(), screen);
}

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

// ---------------------------------------------------------------- 窗口过程

unsafe extern "system" fn frame_proc(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> isize {
    match msg {
        WM_TIMER => {
            on_timer();
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe extern "system" fn hud_proc(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> isize {
    match msg {
        WM_LBUTTONUP => {
            let x = (lparam as usize & 0xFFFF) as u16 as i16 as i32;
            let action = if x >= BTN_CANCEL_X {
                ACT_CANCEL
            } else if x >= BTN_SAVE_X {
                ACT_SAVE
            } else if x >= BTN_COPY_X {
                ACT_COPY
            } else {
                return 0;
            };
            if let Some(s) = S.as_mut() {
                crate::win::trace(&format!("scroll: 控制条动作 {action}"));
                req_close(s, action);
            }
            0
        }
        WM_SETCURSOR => {
            SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_HAND));
            1
        }
        WM_PAINT => {
            // 窗口被遮住再露出来时重画一次，内容取自当前进度。
            // 必须走 BeginPaint/EndPaint 把更新区标记掉，否则会一直收到 WM_PAINT。
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            let (h, f) = S
                .as_ref()
                .map(|s| (s.st.height(), s.st.frames))
                .unwrap_or((0, 0));
            paint_hud_dc(hdc, h, f);
            EndPaint(hwnd, &ps);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
