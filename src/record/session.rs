//! 录制会话：后台线程 + 选区外的红框 + 可点击的悬浮控制条。
//!
//! 录制本体在独立线程里跑（60fps 循环不能占主线程，否则热键和托盘全哑）。
//! 主线程只负责：开录、每秒刷一次托盘和控制条、停止时 join。
//!
//! 两个辅助窗口都不在选区里：
//! - 红框：3px，紧贴选区外侧，点击穿透
//! - 控制条：优先放选区上方，放不下挪到下方，再放不下才贴屏幕底边

use crate::config::Config;
use crate::record::{self, Report};
use crate::win::{hinstance, to_wide};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use windows_sys::Win32::Foundation::{HWND, POINT, RECT, SIZE, SYSTEMTIME};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HGDIOBJ,
    AC_SRC_ALPHA, AC_SRC_OVER,
};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::UI::WindowsAndMessaging::{LoadCursorW, SetCursor, IDC_HAND};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, PostMessageW, RegisterClassW,
    SetWindowDisplayAffinity, ShowWindow, UpdateLayeredWindow, SW_SHOWNOACTIVATE, ULW_ALPHA,
    WDA_EXCLUDEFROMCAPTURE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WM_LBUTTONUP, WM_SETCURSOR, WS_POPUP,
};

const FRAME_CLASS: &str = "SmartPasteRecFrame";
const HUD_CLASS: &str = "SmartPasteRecHud";
/// 红框贴边的厚度。整圈都在选区外面，不会被录进去。
const BORDER: i32 = 3;
/// 直接写像素用（0xAARRGGBB 的 RGB 部分）。
/// 注意：传给 gdiplus::fill_round 时必须用 rgb(r,g,b) 的 COLORREF，不能直接用这个。
const ACCENT: u32 = 0x00FF_2E63;

/// 控制条尺寸（物理像素）
const HUD_W: i32 = 200;
const HUD_H: i32 = 44;
/// 停止按钮的左边界，点右边这片区域就停
const STOP_X: i32 = 108;

/// 主窗口收到它就停止录制。
/// 注意别和 WM_APP+n 的其他消息撞值：WM_TRAY=+1、WM_SNAP_DONE=+2、
/// 设置面板用到 +3/+4，遮罩的文本消息是 +5/+6，这里取 +7。
pub const WM_REC_STOP: u32 = 0x8000 + 7;

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

pub enum Outcome {
    Ok {
        report: Report,
        /// 先写到了临时目录，等主线程弹另存为
        ask_save: bool,
    },
    /// 录了不到 1 秒，文件已删除
    TooShort,
    Err(String),
}

struct Session {
    stop: Arc<AtomicBool>,
    progress: Arc<AtomicU64>,
    join: Option<JoinHandle<windows::core::Result<Report>>>,
    frame: HWND,
    hud: HWND,
    /// 控制条是否成功从捕获里排除了。false 说明系统太老，只能放选区外
    hud_excluded: bool,
    main: HWND,
    rect: RECT,
    path: String,
    ask_save: bool,
}

static mut S: Option<Session> = None;

pub fn register() {
    unsafe {
        let frame_class = to_wide(FRAME_CLASS);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(def_proc),
            hInstance: hinstance(),
            lpszClassName: frame_class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);

        let hud_class = to_wide(HUD_CLASS);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(hud_proc),
            hInstance: hinstance(),
            lpszClassName: hud_class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
    }
}

extern "system" fn def_proc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

extern "system" fn hud_proc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    unsafe {
        match msg {
            WM_LBUTTONUP => {
                let x = (lp as u32 & 0xFFFF) as i16 as i32;
                if x >= STOP_X {
                    if let Some(s) = S.as_ref() {
                        crate::win::trace("rec: 控制条上的停止");
                        PostMessageW(s.main, WM_REC_STOP, 0, 0);
                    }
                }
                0
            }
            WM_SETCURSOR => {
                let hit = (lp as u32) & 0xFFFF;
                if hit == 1 {
                    // HTCLIENT：手型，提示这里能点
                    let hand = LoadCursorW(std::ptr::null_mut(), IDC_HAND);
                    SetCursor(hand);
                    1
                } else {
                    DefWindowProcW(hwnd, msg, wp, lp)
                }
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

pub fn is_recording() -> bool {
    unsafe { S.is_some() }
}

/// 已录秒数，托盘和控制条每秒读一次。
pub fn elapsed_secs() -> u64 {
    unsafe {
        S.as_ref()
            .map(|s| s.progress.load(Ordering::Relaxed))
            .unwrap_or(0)
    }
}

/// 开始录制。`rect` 是屏幕物理像素。
pub fn start(main: HWND, rect: RECT, cfg: &Config) -> Result<(), String> {
    unsafe {
        if S.is_some() {
            return Err("已经在录制了".into());
        }
        let w = rect.right - rect.left;
        let h = rect.bottom - rect.top;
        if w < 128 || h < 128 {
            return Err("选区太小，至少要 128×128".into());
        }

        let (path, ask_save) = video_target(cfg);
        let frame = create_frame(rect).ok_or_else(|| "红框窗口创建失败".to_string())?;
        let (hud, hud_excluded) =
            create_hud(rect).ok_or_else(|| "控制条窗口创建失败".to_string())?;
        crate::win::trace(&format!(
            "record: hud {}",
            if hud_excluded {
                "已从捕获排除，放选区中下方"
            } else {
                "无法排除，放选区外"
            }
        ));

        let stop = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        let opts = record::Options {
            x: rect.left,
            y: rect.top,
            width: w as u32,
            height: h as u32,
            fps: cfg.record_fps,
            path: path.clone(),
            max_seconds: None,
            stop: Arc::clone(&stop),
            progress: Arc::clone(&progress),
        };

        let join = std::thread::spawn(move || record::run(opts));
        crate::win::trace(&format!(
            "record: start {}x{} at ({},{}) {}fps -> {path}",
            w, h, rect.left, rect.top, cfg.record_fps
        ));

        S = Some(Session {
            stop,
            progress,
            join: Some(join),
            frame,
            hud,
            hud_excluded,
            main,
            rect,
            path,
            ask_save,
        });
        Ok(())
    }
}

/// 每秒重画一次控制条上的时长。
pub fn refresh() {
    unsafe {
        if let Some(s) = S.as_ref() {
            let secs = s.progress.load(Ordering::Relaxed);
            paint_hud(s.hud, s.rect, secs, s.hud_excluded);
        }
    }
}

/// 主动停止。阻塞到文件写完（通常 100~500ms）。
pub fn stop() -> Option<Outcome> {
    unsafe {
        let mut s = S.take()?;
        s.stop.store(true, Ordering::Relaxed);
        let res = match s.join.take() {
            Some(j) => j.join(),
            None => return None,
        };
        destroy(s.hud);
        destroy(s.frame);
        Some(judge(res, &s.path, s.ask_save))
    }
}

/// 线程自己结束了（ACCESS_LOST、磁盘满、编码器挂掉）。
pub fn poll() -> Option<Outcome> {
    unsafe {
        let done = S
            .as_ref()
            .map(|s| s.join.as_ref().map(|j| j.is_finished()).unwrap_or(true))
            .unwrap_or(false);
        if !done {
            return None;
        }
    }
    stop()
}

fn judge(
    res: std::thread::Result<windows::core::Result<Report>>,
    path: &str,
    ask_save: bool,
) -> Outcome {
    match res {
        Ok(Ok(r)) => {
            crate::win::trace(&r.summary());
            // 不到 1 秒的录制没有意义，直接丢
            if r.frames < r.fps as u64 {
                let _ = std::fs::remove_file(path);
                Outcome::TooShort
            } else {
                Outcome::Ok {
                    report: r,
                    ask_save,
                }
            }
        }
        Ok(Err(e)) => {
            crate::win::trace(&format!("record failed: {e:?}"));
            let _ = std::fs::remove_file(path);
            Outcome::Err(friendly(&e))
        }
        Err(_) => {
            crate::win::trace("record failed: thread panic");
            let _ = std::fs::remove_file(path);
            Outcome::Err("录制线程异常退出".into())
        }
    }
}

fn friendly(e: &windows::core::Error) -> String {
    let code = e.code().0 as u32;
    match code {
        // DXGI_ERROR_ACCESS_LOST
        0x887A_0026 => "显示器状态变了（改分辨率 / 锁屏 / 拔显示器），录制已停止".into(),
        // HRESULT_FROM_WIN32(ERROR_DISK_FULL) / ERROR_HANDLE_DISK_FULL
        0x8007_0027 | 0x8007_0028 => "磁盘空间不足".into(),
        // E_INVALIDARG：多半是 HDR 屏或选区不合法
        0x8007_0057 => "无法开始录制：选区不合法，或显示器开了 HDR（v1 不支持）".into(),
        _ => format!("录制失败：{}", e.message()),
    }
}

/// 返回到落盘路径，以及"录完是否还要问另存为"。
///
/// 要问的话先写临时目录，等用户选完再搬；不问就直接落到配置的目录。
fn video_target(cfg: &Config) -> (String, bool) {
    let stamp = unsafe {
        let mut st: SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut st);
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
        )
    };
    let name = format!("SmartPaste_{stamp}.mp4");

    if cfg.video_ask_save {
        let path = std::env::temp_dir().join(format!("rec_{name}"));
        return (path.display().to_string(), true);
    }

    let dir = cfg.video_dir();
    let _ = std::fs::create_dir_all(&dir);
    (dir.join(name).display().to_string(), false)
}

unsafe fn destroy(hwnd: HWND) {
    if !hwnd.is_null() {
        DestroyWindow(hwnd);
    }
}

unsafe fn create_frame(rect: RECT) -> Option<HWND> {
    let w = rect.right - rect.left + BORDER * 2;
    let h = rect.bottom - rect.top + BORDER * 2;
    if w <= 0 || h <= 0 {
        return None;
    }

    let class = to_wide(FRAME_CLASS);
    let title = to_wide("SmartPaste 录制中");
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
        class.as_ptr(),
        title.as_ptr(),
        WS_POPUP,
        rect.left - BORDER,
        rect.top - BORDER,
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

    // 直接编一张 32bpp 的边框图，整块提交给 UpdateLayeredWindow
    let (x, y) = (rect.left - BORDER, rect.top - BORDER);
    let screen = GetDC(std::ptr::null_mut());
    if screen.is_null() {
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

    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let bmp: HBITMAP = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
    if bmp.is_null() || bits.is_null() {
        DeleteDC(dc);
        ReleaseDC(std::ptr::null_mut(), screen);
        return None;
    }
    let old = SelectObject(dc, bmp as HGDIOBJ);

    let px = std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);
    px.fill(0);
    for row in 0..h {
        for col in 0..w {
            let edge = col < BORDER || row < BORDER || col >= w - BORDER || row >= h - BORDER;
            if edge {
                px[(row * w + col) as usize] = 0xFF00_0000 | ACCENT;
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

/// 控制条放哪儿。
///
/// - 能从捕获里排除（Win10 2004+）：固定在选区水平居中、距底边 24px，
///   录制画面里看不到它
/// - 排不掉：退回原来的"选区上方 → 下方 → 屏幕底边"，宁可放外面也不挡内容
fn hud_pos(rect: RECT, excluded: bool) -> (i32, i32) {
    let (vx, vy, vw, vh) = crate::capture::desktop_bounds();

    if excluded {
        let x = rect.left + (rect.right - rect.left - HUD_W) / 2;
        let y = rect.bottom - HUD_H - 24;
        return (
            x.clamp(vx, (vx + vw - HUD_W).max(vx)),
            y.clamp(vy, (vy + vh - HUD_H).max(vy)),
        );
    }

    let x = rect.left.clamp(vx, (vx + vw - HUD_W).max(vx));
    let mut y = rect.top - HUD_H - 10;
    if y < vy {
        y = rect.bottom + 10;
    }
    if y + HUD_H > vy + vh {
        y = vy + vh - HUD_H - 10;
    }
    (x, y)
}

unsafe fn create_hud(rect: RECT) -> Option<(HWND, bool)> {
    let (x, y) = hud_pos(rect, false);
    let class = to_wide(HUD_CLASS);
    let title = to_wide("SmartPaste 录制控制");
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
        class.as_ptr(),
        title.as_ptr(),
        WS_POPUP,
        x,
        y,
        HUD_W,
        HUD_H,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        hinstance(),
        std::ptr::null(),
    );
    if hwnd.is_null() {
        return None;
    }

    // Win10 2004+ 才有 WDA_EXCLUDEFROMCAPTURE；旧系统返回失败，退回选区外
    let excluded = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) != 0;

    paint_hud(hwnd, rect, 0, excluded);
    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    Some((hwnd, excluded))
}

/// 重画控制条。背景圆角 + 红点 + 时长 + 停止按钮，整张走预乘 alpha。
unsafe fn paint_hud(hwnd: HWND, rect: RECT, secs: u64, excluded: bool) {
    if hwnd.is_null() {
        return;
    }
    let screen = GetDC(std::ptr::null_mut());
    if screen.is_null() {
        return;
    }
    let dc = CreateCompatibleDC(screen);
    if dc.is_null() {
        ReleaseDC(std::ptr::null_mut(), screen);
        return;
    }

    let mut bi: BITMAPINFO = std::mem::zeroed();
    bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = HUD_W;
    bi.bmiHeader.biHeight = -HUD_H;
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB;

    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let bmp: HBITMAP = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
    if bmp.is_null() || bits.is_null() {
        DeleteDC(dc);
        ReleaseDC(std::ptr::null_mut(), screen);
        return;
    }
    let old = SelectObject(dc, bmp as HGDIOBJ);

    let px = std::slice::from_raw_parts_mut(bits as *mut u32, (HUD_W * HUD_H) as usize);
    px.fill(0);

    let panel = RECT {
        left: 0,
        top: 0,
        right: HUD_W,
        bottom: HUD_H,
    };
    crate::gdiplus::fill_round(dc, &panel, 10.0, rgb(0x14, 0x14, 0x18));

    // 红点一秒一亮一暗，不额外开定时器
    let dot = if secs % 2 == 0 {
        rgb(0xFF, 0x2E, 0x63)
    } else {
        rgb(0x7A, 0x16, 0x31)
    };
    let dot_r = RECT {
        left: 14,
        top: 17,
        right: 24,
        bottom: 27,
    };
    crate::gdiplus::fill_round(dc, &dot_r, 5.0, dot);

    let f_time = crate::gdiplus::FontSpec::new(13.0, true, "Segoe UI");
    let label = format!("{:02}:{:02}", secs / 60, secs % 60);
    crate::gdiplus::draw_text(dc, &f_time, rgb(0xEC, 0xEC, 0xF2), 32, 13, &label);

    let btn = RECT {
        left: 112,
        top: 8,
        right: 192,
        bottom: 36,
    };
    crate::gdiplus::fill_round(dc, &btn, 7.0, rgb(0xFF, 0x2E, 0x63));
    let f_btn = crate::gdiplus::FontSpec::new(12.5, true, "Microsoft YaHei");
    crate::gdiplus::draw_text(dc, &f_btn, rgb(0xFF, 0xFF, 0xFF), 138, 15, "停止");

    let (x, y) = hud_pos(rect, excluded);
    blit_layered(hwnd, screen, dc, x, y, HUD_W, HUD_H);

    SelectObject(dc, old);
    DeleteObject(bmp as HGDIOBJ);
    DeleteDC(dc);
    ReleaseDC(std::ptr::null_mut(), screen);
}

unsafe fn blit_layered(hwnd: HWND, screen: windows_sys::Win32::Graphics::Gdi::HDC, dc: windows_sys::Win32::Graphics::Gdi::HDC, x: i32, y: i32, w: i32, h: i32) {
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
