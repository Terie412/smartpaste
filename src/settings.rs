//! 设置窗口。
//!
//! 自绘面板，不用对话框资源。目前只有一项：截图热键。
//! 点一下快捷键键帽进入录制，按下组合键即生效并落盘。
//!
//! 标题栏也是自绘的：原生 caption 的关闭按钮在 Win11 上是直角矩形，
//! 改不了圆角，所以把非客户区整个去掉（WM_NCCALCSIZE 返回 0），
//! 自己画标题和关闭按钮。窗口本身仍保留 WS_CAPTION，DWM 的阴影和
//! 圆角才会继续生效。

use crate::config::{
    hotkey_parts, Config, DEFAULT_MOD, DEFAULT_VK, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN,
};
use crate::gdiplus::FontSpec;
use crate::win::{hinstance, pt_in, rect, to_wide};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DeleteDC,
    DeleteObject, EndPaint, FillRect, InvalidateRect, ScreenToClient, SelectObject, SetBkMode, HDC,
    HGDIOBJ, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetSystemMetrics, LoadCursorW,
    PostMessageW, RegisterClassW, SetCursor, SetForegroundWindow, SetWindowPos, ShowWindow,
    CS_HREDRAW, CS_VREDRAW, HCURSOR, HTCAPTION, HTCLIENT, IDC_ARROW, IDC_HAND, SM_CXSCREEN,
    SM_CYSCREEN, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_SHOW, WM_APP,
    WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_NCCALCSIZE,
    WM_NCHITTEST, WM_PAINT, WM_SETCURSOR, WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW, WS_CAPTION,
    WS_EX_TOOLWINDOW, WS_OVERLAPPED, WS_SYSMENU,
};

#[link(name = "dwmapi")]
extern "system" {
    fn DwmSetWindowAttribute(
        hwnd: HWND,
        attr: u32,
        value: *const core::ffi::c_void,
        size: u32,
    ) -> i32;
}

const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
const DWMWCP_ROUND: i32 = 2;

const CLASS: &str = "SmartPasteSettings";

/// windows-sys 0.59 没导出这个常量
const WM_MOUSELEAVE: u32 = 0x02A3;

/// 热键改完之后通知主窗口重注册
pub const WM_HOTKEY_CHANGED: u32 = WM_APP + 3;
/// 开始录制，让主窗口先把热键注销掉
pub const WM_RECORD_START: u32 = WM_APP + 4;

// 设置面板按 125% 缩放（对齐系统 DPI）。
// 遮罩那边用的是 1.6 倍——它是截图时的临时 UI，可以大；设置是常态窗口，要贴近系统字号。
const UI_SCALE: (i32, i32) = (5, 4);
const fn ui(v: i32) -> i32 {
    v * UI_SCALE.0 / UI_SCALE.1
}

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

// ---- 色板：直接取自应用图标 ----
// 图标是玫红底 + 左上金黄 + 右下天青。界面只用玫红一种强调色，而且大块面
// 就是图标原色 #FF2E63，不再压深——压深会在视觉上和图标对不上。
// 只有两种场合要换档：浅底上的小字（原色对比度不够），以及亮粉块上的深色块。
// 金黄只做「正在录制」的描边和顶部色带，天青只出现在色带里。
const ROSE: u32 = rgb(0xFF, 0x2E, 0x63); // 图标主色
const YELLOW: u32 = rgb(0xFF, 0xC3, 0x00);
const CYAN: u32 = rgb(0x00, 0xC8, 0xFF);

const BG: u32 = rgb(0xFA, 0xF7, 0xF8);
const DIVIDER: u32 = rgb(0xEE, 0xE3, 0xE7);

const TXT_1: u32 = rgb(0x22, 0x1A, 0x1D); // 主文字
const TXT_2: u32 = rgb(0x6B, 0x5E, 0x63); // 次文字
const TXT_3: u32 = rgb(0x96, 0x89, 0x8E); // 辅助文字

const ACCENT: u32 = ROSE; // 深色主区、选中项填充
/// 同色系压一档：浅底上的 12px 分节标题，以及亮粉块里的胶囊底。
const ACCENT_DEEP: u32 = rgb(0xD9, 0x0A, 0x42);
const ON_ACCENT: u32 = rgb(0xFF, 0xFF, 0xFF);

const HERO_BG: u32 = ACCENT;
const HERO_HOVER: u32 = ACCENT_DEEP;
const HERO_TXT: u32 = rgb(0xFF, 0xFF, 0xFF);
const HERO_MUTED: u32 = rgb(0xFF, 0xF0, 0xF4);
const HERO_ACCENT: u32 = YELLOW;
const HERO_OFF: u32 = rgb(0xFF, 0x9F, 0xBA);

const KEY_BG: u32 = rgb(0xFF, 0xFF, 0xFF);
const KEY_BG_HOVER: u32 = rgb(0xFF, 0xE9, 0xEF);
const KEY_BORDER: u32 = rgb(0xEC, 0xDA, 0xE0);
const KEY_BORDER_HOVER: u32 = ROSE;
const KEY_TXT: u32 = rgb(0x2A, 0x1A, 0x20);
const PLUS: u32 = rgb(0xFF, 0xC6, 0xD6);
const HOVER_BG: u32 = rgb(0xF6, 0xEC, 0xEF);
const SEGMENT_BG: u32 = rgb(0xF1, 0xE7, 0xEA);

/// 描边宽度要跟着缩放走。写死 1px 的话，16px 半径的圆角上会断成一节一节。
const HAIRLINE: f32 = 1.6;

// ---- 布局 ----
const W: i32 = ui(480);
const H: i32 = ui(598);

const HEADER_H: i32 = ui(66);
const PAD: i32 = ui(28);
const CLOSE: i32 = ui(32);
const CR: i32 = W - PAD;

// 截图快捷键是主操作，独占一个深色区域；其余设置用留白和分隔线分组。
const HOT_CARD: RECT = rect(PAD, ui(76), W - PAD, ui(204));
const HOT_CX: i32 = PAD + ui(20);
const HOT_CR: i32 = W - PAD - ui(20);
const HOT_TITLE_Y: i32 = ui(88);
const HOT_DESC_Y: i32 = ui(109);
const HOT_KEY_CY: i32 = ui(166);
const RESET_CY: i32 = ui(98);

const RECORD_HEAD_Y: i32 = ui(222);
const FPS_CY: i32 = ui(266);
const DIV_RECORD_Y: i32 = ui(293);
const CD_CY: i32 = ui(320);

const SAVE_HEAD_Y: i32 = ui(360);
const SAVE_CY: i32 = ui(400);
const DIV_SAVE_Y: i32 = ui(427);
const ASK_CY: i32 = ui(454);
const DIV_OPEN_Y: i32 = ui(481);
const OPEN_CY: i32 = ui(508);
const FOOTER_Y: i32 = ui(574);

const KEY_H: i32 = ui(36);
const KEY_GAP: i32 = ui(20);
const KEY_PAD_X: i32 = ui(12);

const fn close_rect() -> RECT {
    rect(
        W - PAD - CLOSE,
        (HEADER_H - CLOSE) / 2,
        W - PAD,
        (HEADER_H + CLOSE) / 2,
    )
}

struct State {
    hwnd: HWND,
    parent: HWND,
    cfg: Config,
    recording: bool,
    pend_vk: u32,
    pend_mod: u32,
    /// 最近一次读到的修饰键状态。Alt 组合的主键 keydown 常被系统吞掉，
    /// 靠这个值在 keyup 时兜底。
    live_mod: u32,
    hover_hot: bool,
    hover_reset: bool,
    hover_close: bool,
    tracking: bool,
    hot_rect: RECT,
    reset_rect: RECT,
    fps30: RECT,
    fps60: RECT,
    cd0: RECT,
    cd3: RECT,
    dir_edit: RECT,
    dir_open: RECT,
    ask_yes: RECT,
    ask_no: RECT,
    open_yes: RECT,
    open_no: RECT,
    hand: HCURSOR,

    f_title: FontSpec,
    f_brand: FontSpec,
    f_h1: FontSpec,
    f_body: FontSpec,
    f_key: FontSpec,
    f_plus: FontSpec,
    f_action: FontSpec,
}

static mut S: Option<Box<State>> = None;

/// 用 GetAsyncKeyState 而不是 GetKeyState：
/// 前者读的是系统级物理状态，注入的按键也认；
/// 后者依赖本线程队列的处理进度，录制时容易读到 0。
fn modifiers() -> u32 {
    let down = |vk: i32| unsafe { GetAsyncKeyState(vk) < 0 };
    let mut m = 0;
    if down(0x11) {
        m |= MOD_CONTROL; // VK_CONTROL
    }
    if down(0x10) {
        m |= MOD_SHIFT; // VK_SHIFT
    }
    if down(0x12) {
        m |= MOD_ALT; // VK_MENU
    }
    if down(0x5B) || down(0x5C) {
        m |= MOD_WIN; // 左右 Win
    }
    m
}

fn is_modifier(vk: u32) -> bool {
    matches!(vk, 0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0xA0..=0xA5)
}

fn is_default(cfg: &Config) -> bool {
    cfg.hotkey_mod == DEFAULT_MOD && cfg.hotkey_vk == DEFAULT_VK
}

fn expand(r: &RECT, d: i32) -> RECT {
    rect(r.left - d, r.top - d, r.right + d, r.bottom + d)
}

pub fn open(parent: HWND) {
    unsafe {
        // 已经开着就前置，不重复创建
        if let Some(s) = S.as_ref() {
            SetForegroundWindow(s.hwnd);
            return;
        }

        let class = to_wide(CLASS);
        let title = to_wide("smartpaste 设置");

        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let x = (sw - W) / 2;
        let y = (sh - H) / 2;

        // 客户区就是整个窗口：WM_NCCALCSIZE 会把标题栏和边框都去掉，
        // 所以这里直接传设计尺寸，不用 AdjustWindowRectEx
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            x,
            y,
            W,
            H,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance(),
            std::ptr::null_mut(),
        );
        if hwnd.is_null() {
            crate::win::trace("settings: CreateWindow failed");
            return;
        }

        // 创建完成后强制重算一次非客户区，确保标题栏被彻底去掉
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
        );

        // 显式要圆角。有 manifest 时默认就是圆的，这里只是兜底。
        let pref: i32 = DWMWCP_ROUND;
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const i32 as *const core::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        );

        S = Some(Box::new(State {
            hwnd,
            parent,
            cfg: Config::load(),
            recording: false,
            pend_vk: 0,
            pend_mod: 0,
            live_mod: 0,
            hover_hot: false,
            hover_reset: false,
            hover_close: false,
            tracking: false,
            hot_rect: rect(0, 0, 0, 0),
            reset_rect: rect(0, 0, 0, 0),
            fps30: rect(0, 0, 0, 0),
            fps60: rect(0, 0, 0, 0),
            cd0: rect(0, 0, 0, 0),
            cd3: rect(0, 0, 0, 0),
            dir_edit: rect(0, 0, 0, 0),
            dir_open: rect(0, 0, 0, 0),
            ask_yes: rect(0, 0, 0, 0),
            ask_no: rect(0, 0, 0, 0),
            open_yes: rect(0, 0, 0, 0),
            open_no: rect(0, 0, 0, 0),
            hand: LoadCursorW(std::ptr::null_mut(), IDC_HAND),

            f_title: FontSpec::new(ui(24) as f32, true, "Microsoft YaHei"),
            f_brand: FontSpec::new(ui(10) as f32, true, "Segoe UI"),
            f_h1: FontSpec::new(ui(14) as f32, true, "Microsoft YaHei"),
            f_body: FontSpec::new(ui(12) as f32, false, "Microsoft YaHei"),
            f_key: FontSpec::new(ui(15) as f32, true, "Segoe UI"),
            f_plus: FontSpec::new(ui(13) as f32, false, "Segoe UI"),
            f_action: FontSpec::new(ui(12) as f32, true, "Microsoft YaHei"),
        }));

        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        SetFocus(hwnd);
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        // 这两条必须在取 S 之前处理：WM_NCCALCSIZE 在 CreateWindowExW 内部就会发一次，
        // 那时 S 还没建好，交给 DefWindowProc 就会把原生标题栏算进去，再也去不掉。
        match msg {
            // 去掉非客户区，但窗口仍带 WS_CAPTION —— DWM 的圆角和阴影还在。
            // 创建时 wParam 是 FALSE，只挡 TRUE 的话初始标题栏会留下来，所以两种都返回 0。
            WM_NCCALCSIZE => return 0,
            // 标题栏区域交给系统拖动，关闭按钮留给自己
            WM_NCHITTEST => {
                let mut pt = POINT {
                    x: lo(lparam),
                    y: hi(lparam),
                };
                ScreenToClient(hwnd, &mut pt);
                return if pt_in(&close_rect(), pt.x, pt.y) || pt.y >= HEADER_H {
                    HTCLIENT as LRESULT
                } else {
                    HTCAPTION as LRESULT
                };
            }
            _ => {}
        }

        let Some(s) = S.as_mut() else {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        };

        match msg {
            WM_PAINT => {
                paint(s);
                0
            }
            WM_ERASEBKGND => 1,
            WM_MOUSEMOVE => {
                let (x, y) = (lo(lparam), hi(lparam));
                let hot = pt_in(&s.hot_rect, x, y);
                let reset = !is_default(&s.cfg) && pt_in(&s.reset_rect, x, y);
                let close = pt_in(&close_rect(), x, y);
                if hot != s.hover_hot || reset != s.hover_reset || close != s.hover_close {
                    s.hover_hot = hot;
                    s.hover_reset = reset;
                    s.hover_close = close;
                    // WM_SETCURSOR 在鼠标停下时不会再来，这里直接设定
                    if hot || reset || close {
                        SetCursor(s.hand);
                    }
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                if (hot || reset || close) && !s.tracking {
                    let mut t: TRACKMOUSEEVENT = std::mem::zeroed();
                    t.cbSize = std::mem::size_of::<TRACKMOUSEEVENT>() as u32;
                    t.dwFlags = TME_LEAVE;
                    t.hwndTrack = hwnd;
                    TrackMouseEvent(&mut t);
                    s.tracking = true;
                }
                0
            }
            WM_MOUSELEAVE => {
                s.tracking = false;
                if s.hover_hot || s.hover_reset || s.hover_close {
                    s.hover_hot = false;
                    s.hover_reset = false;
                    s.hover_close = false;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                0
            }
            WM_SETCURSOR => {
                let hit = (lparam as u32) & 0xFFFF;
                if hit == HTCLIENT && (s.hover_hot || s.hover_reset || s.hover_close) {
                    SetCursor(s.hand);
                    1
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            WM_LBUTTONDOWN => {
                let (x, y) = (lo(lparam), hi(lparam));
                // 点击就是「用户输入」，这时抢前台才被系统允许，
                // 否则窗口只在后台，键盘消息根本进不来
                SetForegroundWindow(hwnd);
                SetFocus(hwnd);

                if pt_in(&close_rect(), x, y) {
                    DestroyWindow(hwnd);
                } else if pt_in(&s.hot_rect, x, y) {
                    if !s.recording {
                        s.recording = true;
                        s.pend_vk = 0;
                        s.pend_mod = 0;
                        s.live_mod = 0;
                        // 录制期间必须先把热键摘掉，否则按到当前生效的组合
                        // 会直接被系统拿去触发截图，窗口收不到按键
                        if !s.parent.is_null() {
                            PostMessageW(s.parent, WM_RECORD_START, 0, 0);
                        }
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                } else if pt_in(&s.fps30, x, y) {
                    if s.cfg.record_fps != 30 {
                        s.cfg.record_fps = 30;
                        s.cfg.save();
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                } else if pt_in(&s.fps60, x, y) {
                    if s.cfg.record_fps != 60 {
                        s.cfg.record_fps = 60;
                        s.cfg.save();
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                } else if pt_in(&s.cd0, x, y) {
                    if s.cfg.record_countdown != 0 {
                        s.cfg.record_countdown = 0;
                        s.cfg.save();
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                } else if pt_in(&s.cd3, x, y) {
                    if s.cfg.record_countdown != 3 {
                        s.cfg.record_countdown = 3;
                        s.cfg.save();
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                } else if pt_in(&s.dir_edit, x, y) {
                    if let Some(d) = crate::explorer::pick_dir(hwnd) {
                        s.cfg.video_dir = Some(d);
                        s.cfg.save();
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                } else if pt_in(&s.dir_open, x, y) {
                    crate::explorer::open_dir(&s.cfg.video_dir().display().to_string());
                } else if pt_in(&s.ask_yes, x, y) {
                    if !s.cfg.video_ask_save {
                        s.cfg.video_ask_save = true;
                        s.cfg.save();
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                } else if pt_in(&s.ask_no, x, y) {
                    if s.cfg.video_ask_save {
                        s.cfg.video_ask_save = false;
                        s.cfg.save();
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                } else if pt_in(&s.open_yes, x, y) {
                    if !s.cfg.video_open_dir {
                        s.cfg.video_open_dir = true;
                        s.cfg.save();
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                } else if pt_in(&s.open_no, x, y) {
                    if s.cfg.video_open_dir {
                        s.cfg.video_open_dir = false;
                        s.cfg.save();
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                } else if !s.recording && !is_default(&s.cfg) && pt_in(&s.reset_rect, x, y) {
                    s.cfg = Config {
                        record_fps: s.cfg.record_fps,
                        record_countdown: s.cfg.record_countdown,
                        video_dir: s.cfg.video_dir.clone(),
                        ..Config::default()
                    };
                    s.cfg.save();
                    crate::win::trace("settings: reset to default");
                    if !s.parent.is_null() {
                        PostMessageW(s.parent, WM_HOTKEY_CHANGED, 0, 0);
                    }
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                0
            }
            // Alt + 键走的是 WM_SYSKEYDOWN，不是 WM_KEYDOWN；
            // 只处理后者的话录不到任何带 Alt 的组合
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let vk = wparam as u32;
                if s.recording {
                    // 修饰键状态在每次按键都刷新，哪怕主键的 down 被吞也有记录
                    s.live_mod = modifiers();
                    if !is_modifier(vk) {
                        s.pend_vk = vk;
                        // 修饰键要在按下这一刻取，等到 KEYUP 时可能已经松了
                        s.pend_mod = s.live_mod;
                    }
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                } else if vk == VK_ESCAPE as u32 {
                    DestroyWindow(hwnd);
                }
                0
            }
            WM_KEYUP | WM_SYSKEYUP => {
                if s.recording {
                    let vk = wparam as u32;
                    if !is_modifier(vk) {
                        // 主键的 keydown 可能被系统吞了（Alt 组合常见），
                        // 这里用 keyup 带回的 vk 和刚才记下的修饰键兜底
                        let pv = if s.pend_vk != 0 { s.pend_vk } else { vk };
                        let pm = if s.pend_mod != 0 {
                            s.pend_mod
                        } else {
                            s.live_mod
                        };
                        if pm != 0 {
                            apply_hotkey(s, pv, pm);
                        }
                    }
                    s.recording = false;
                    s.pend_vk = 0;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    // 不管录没录到，都让主窗口按配置重新注册一次
                    if !s.parent.is_null() {
                        PostMessageW(s.parent, WM_HOTKEY_CHANGED, 0, 0);
                    }
                }
                0
            }
            WM_DESTROY => {
                S = None;
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

fn lo(l: LPARAM) -> i32 {
    (l as usize & 0xFFFF) as u16 as i16 as i32
}

fn hi(l: LPARAM) -> i32 {
    ((l as usize >> 16) & 0xFFFF) as u16 as i16 as i32
}

/// 落盘 + 通知主窗口重注册热键
unsafe fn apply_hotkey(s: &mut State, vk: u32, mods: u32) {
    s.cfg.hotkey_mod = mods;
    s.cfg.hotkey_vk = vk;
    s.cfg.save();
    crate::win::trace(&format!("settings: hotkey -> {}", s.cfg.hotkey_text()));
}

// ---- 绘制小工具 ----

unsafe fn solid(dc: HDC, r: &RECT, color: u32) {
    let b = CreateSolidBrush(color);
    FillRect(dc, r, b);
    DeleteObject(b as HGDIOBJ);
}

unsafe fn measure(dc: HDC, font: FontSpec, s: &str) -> (i32, i32) {
    crate::gdiplus::measure_text(dc, &font, s)
}

unsafe fn text(dc: HDC, font: FontSpec, color: u32, x: i32, y: i32, s: &str) {
    crate::gdiplus::draw_text(dc, &font, color, x, y, s);
}

/// 以 (cx, cy) 为中心画一行字
unsafe fn text_at(dc: HDC, font: FontSpec, color: u32, cx: i32, cy: i32, s: &str) {
    let (w, h) = measure(dc, font, s);
    text(dc, font, color, cx - w / 2, cy - h / 2, s);
}

/// 常驻的小按钮（更改 / 打开），没有选中态。
unsafe fn draw_button(dc: HDC, r: &RECT, label: &str, font: FontSpec) {
    crate::gdiplus::fill_round(dc, r, ui(8) as f32, KEY_BG);
    crate::gdiplus::stroke_round(dc, r, ui(8) as f32, KEY_BORDER, HAIRLINE);
    text_at(
        dc,
        font,
        TXT_1,
        (r.left + r.right) / 2,
        (r.top + r.bottom) / 2,
        label,
    );
}

/// 路径太长时从中间省略，保留盘符和末级目录。
fn ellipsize(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let head = max.saturating_sub(1) / 2;
    let tail = max.saturating_sub(1) - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    out
}

/// 计算一组二选一分段控件的矩形，两个选项等宽。
unsafe fn segment_rects(
    dc: HDC,
    right: i32,
    cy: i32,
    left_label: &str,
    right_label: &str,
    font: FontSpec,
) -> (RECT, RECT) {
    let w = measure(dc, font, left_label)
        .0
        .max(measure(dc, font, right_label).0)
        + ui(24);
    let h = ui(34);
    let gap = ui(4);
    let right_rect = rect(right - w, cy - h / 2, right, cy + h / 2);
    let left_rect = rect(right - w * 2 - gap, cy - h / 2, right - w - gap, cy + h / 2);
    (left_rect, right_rect)
}

/// 分段控件共用一个浅底轨道，选中项是实心墨绿，不再各自描边。
unsafe fn draw_segment_track(dc: HDC, left: &RECT, right: &RECT) {
    let pad = ui(3);
    let track = rect(
        left.left - pad,
        left.top - pad,
        right.right + pad,
        right.bottom + pad,
    );
    crate::gdiplus::fill_round(dc, &track, ui(10) as f32, SEGMENT_BG);
}

unsafe fn draw_chip(dc: HDC, r: &RECT, label: &str, active: bool, font: FontSpec) {
    if active {
        crate::gdiplus::fill_round(dc, r, ui(8) as f32, ACCENT);
    }
    let color = if active { ON_ACCENT } else { TXT_2 };
    text_at(
        dc,
        font,
        color,
        (r.left + r.right) / 2,
        (r.top + r.bottom) / 2,
        label,
    );
}

/// 按给定内边距算出每个键帽的宽度和总宽
unsafe fn key_widths(dc: HDC, font: FontSpec, parts: &[String], pad: i32) -> (Vec<i32>, i32) {
    let mut widths = Vec::with_capacity(parts.len());
    let mut total = 0;
    for p in parts {
        let w = measure(dc, font, p).0 + pad * 2;
        total += w;
        widths.push(w);
    }
    (widths, total)
}

/// 键帽组在深色区域底部居中；返回用于点击判定的外扩矩形。
/// 组合较长时先压缩键帽内边距和间距，避免超出内容区域。
unsafe fn draw_keycaps(s: &State, dc: HDC, avail: i32) -> RECT {
    let parts = hotkey_parts(s.cfg.hotkey_mod, s.cfg.hotkey_vk);

    let (mut widths, mut total) = key_widths(dc, s.f_key, &parts, KEY_PAD_X);
    let mut gap = KEY_GAP;
    total += gap * (parts.len() as i32 - 1);
    if total > avail {
        let (w, t) = key_widths(dc, s.f_key, &parts, ui(7));
        widths = w;
        gap = ui(12);
        total = t + gap * (parts.len() as i32 - 1);
    }

    let top = HOT_KEY_CY - KEY_H / 2;
    let (bg, border) = if s.hover_hot {
        (KEY_BG_HOVER, KEY_BORDER_HOVER)
    } else {
        (KEY_BG, KEY_BORDER)
    };

    let left = HOT_CX + (avail - total).max(0) / 2;
    let mut x = left;
    for (i, p) in parts.iter().enumerate() {
        let r = rect(x, top, x + widths[i], top + KEY_H);
        crate::gdiplus::fill_round(dc, &r, ui(8) as f32, bg);
        crate::gdiplus::stroke_round(dc, &r, ui(8) as f32, border, HAIRLINE);
        text_at(dc, s.f_key, KEY_TXT, (r.left + r.right) / 2, HOT_KEY_CY, p);

        x += widths[i];
        if i + 1 < parts.len() {
            text_at(dc, s.f_plus, PLUS, x + gap / 2, HOT_KEY_CY, "+");
            x += gap;
        }
    }

    expand(&rect(left, top, left + total, top + KEY_H), ui(6))
}

/// 整窗先画到内存位图，再一次性提交。
///
/// 直接往窗口 DC 上逐层画时，背景填充和控件之间会出现可见的中间帧。
/// 旧界面底色与卡片颜色接近，所以不明显；高对比的深色区域会把它放大。
unsafe fn paint(s: &mut State) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(s.hwnd, &mut ps);

    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(s.hwnd, &mut rc);
    let width = rc.right - rc.left;
    let height = rc.bottom - rc.top;

    if width > 0 && height > 0 {
        let mem = CreateCompatibleDC(hdc);
        let bitmap = if mem.is_null() {
            std::ptr::null_mut()
        } else {
            CreateCompatibleBitmap(hdc, width, height)
        };

        if !mem.is_null() && !bitmap.is_null() {
            let old = SelectObject(mem, bitmap);
            paint_content(s, mem, &rc);
            BitBlt(hdc, 0, 0, width, height, mem, 0, 0, SRCCOPY);
            SelectObject(mem, old);
            DeleteObject(bitmap as HGDIOBJ);
            DeleteDC(mem);
        } else {
            // 分配失败时宁可退回直接绘制，也不能让窗口空白。
            if !mem.is_null() {
                DeleteDC(mem);
            }
            paint_content(s, hdc, &rc);
        }
    }

    EndPaint(s.hwnd, &ps);
}

unsafe fn paint_content(s: &mut State, hdc: HDC, rc: &RECT) {
    solid(hdc, rc, BG);
    SetBkMode(hdc, TRANSPARENT as i32);

    // ---- 自绘标题栏 ----
    // 顶上一条三色带，就是图标的那三种颜色，用它代替编造出来的 logo 图形。
    let band_h = ui(4);
    let b1 = (W as f32 * 0.52) as i32;
    let b2 = (W as f32 * 0.80) as i32;
    crate::gdiplus::rect_fill(hdc, &rect(0, 0, b1, band_h), ROSE);
    crate::gdiplus::rect_fill(hdc, &rect(b1, 0, b2, band_h), YELLOW);
    crate::gdiplus::rect_fill(hdc, &rect(b2, 0, W, band_h), CYAN);

    text(hdc, s.f_title, TXT_1, PAD, ui(14), "设置");
    text(hdc, s.f_brand, TXT_3, PAD + ui(1), ui(45), "SMARTPASTE");

    let close = close_rect();
    if s.hover_close {
        crate::gdiplus::fill_round(hdc, &close, ui(8) as f32, HOVER_BG);
    }
    let cx = (close.left + close.right) / 2;
    let cy = (close.top + close.bottom) / 2;
    let d = ui(5);
    let cross = if s.hover_close { TXT_1 } else { TXT_2 };
    crate::gdiplus::line(hdc, cx - d, cy - d, cx + d, cy + d, cross, HAIRLINE);
    crate::gdiplus::line(hdc, cx - d, cy + d, cx + d, cy - d, cross, HAIRLINE);

    // ---- 截图快捷键 ----
    crate::gdiplus::fill_round(hdc, &HOT_CARD, ui(14) as f32, HERO_BG);
    text(hdc, s.f_h1, HERO_TXT, HOT_CX, HOT_TITLE_Y, "截图快捷键");

    let (desc, desc_color) = if s.recording {
        ("按下组合键，Esc 取消", HERO_TXT)
    } else {
        ("点击键帽后按下新的组合键", HERO_MUTED)
    };
    text(hdc, s.f_body, desc_color, HOT_CX, HOT_DESC_Y, desc);

    let default = is_default(&s.cfg);
    let reset_label = "恢复默认";
    let (reset_w, _) = measure(hdc, s.f_action, reset_label);
    let reset_bw = reset_w + ui(18);
    let reset_bh = ui(28);
    let reset = rect(
        HOT_CR - reset_bw,
        RESET_CY - reset_bh / 2,
        HOT_CR,
        RESET_CY + reset_bh / 2,
    );
    s.reset_rect = reset;
    if !default && s.hover_reset {
        crate::gdiplus::fill_round(hdc, &reset, ui(7) as f32, HERO_HOVER);
    }
    let reset_color = if default {
        HERO_OFF
    } else if s.hover_reset {
        HERO_TXT
    } else {
        HERO_MUTED
    };
    text_at(
        hdc,
        s.f_action,
        reset_color,
        (reset.left + reset.right) / 2,
        RESET_CY,
        reset_label,
    );

    let avail = HOT_CR - HOT_CX;
    if s.recording {
        let pill_w = ui(210);
        let pill_h = ui(36);
        let left = HOT_CX + (avail - pill_w) / 2;
        let pill = rect(
            left,
            HOT_KEY_CY - pill_h / 2,
            left + pill_w,
            HOT_KEY_CY + pill_h / 2,
        );
        crate::gdiplus::fill_round(hdc, &pill, ui(10) as f32, HERO_HOVER);
        crate::gdiplus::stroke_round(hdc, &pill, ui(10) as f32, HERO_ACCENT, HAIRLINE);
        text_at(
            hdc,
            s.f_action,
            HERO_TXT,
            (pill.left + pill.right) / 2,
            HOT_KEY_CY,
            "正在等待输入…",
        );
        s.hot_rect = expand(&pill, ui(6));
    } else {
        s.hot_rect = draw_keycaps(s, hdc, avail);
    }

    // ---- 录制设置：用分节和细分隔线代替重复卡片 ----
    text(hdc, s.f_action, ACCENT_DEEP, PAD, RECORD_HEAD_Y, "录制");

    let (_, row_title_h) = measure(hdc, s.f_h1, "录制帧率");
    let (_, row_body_h) = measure(hdc, s.f_body, "60 更流畅");

    let top = FPS_CY - (row_title_h + row_body_h) / 2;
    text(hdc, s.f_h1, TXT_1, PAD, top, "录制帧率");
    text(
        hdc,
        s.f_body,
        TXT_2,
        PAD,
        top + row_title_h,
        "60 更流畅，30 更省资源",
    );

    let (fps30, fps60) = segment_rects(hdc, CR, FPS_CY, "30 fps", "60 fps", s.f_body);
    s.fps30 = fps30;
    s.fps60 = fps60;
    draw_segment_track(hdc, &fps30, &fps60);
    draw_chip(hdc, &fps30, "30 fps", s.cfg.record_fps == 30, s.f_body);
    draw_chip(hdc, &fps60, "60 fps", s.cfg.record_fps == 60, s.f_body);

    solid(hdc, &rect(PAD, DIV_RECORD_Y, CR, DIV_RECORD_Y + 1), DIVIDER);

    let top = CD_CY - (row_title_h + row_body_h) / 2;
    text(hdc, s.f_h1, TXT_1, PAD, top, "开始前倒计时");
    text(
        hdc,
        s.f_body,
        TXT_2,
        PAD,
        top + row_title_h,
        "留出切窗口的时间",
    );

    let (cd0, cd3) = segment_rects(hdc, CR, CD_CY, "立即", "3 秒", s.f_body);
    s.cd0 = cd0;
    s.cd3 = cd3;
    draw_segment_track(hdc, &cd0, &cd3);
    draw_chip(hdc, &cd0, "立即", s.cfg.record_countdown == 0, s.f_body);
    draw_chip(hdc, &cd3, "3 秒", s.cfg.record_countdown == 3, s.f_body);

    // ---- 保存设置 ----
    text(hdc, s.f_action, ACCENT_DEEP, PAD, SAVE_HEAD_Y, "保存");

    let top = SAVE_CY - (row_title_h + row_body_h) / 2;
    text(hdc, s.f_h1, TXT_1, PAD, top, "保存位置");
    let dir = s.cfg.video_dir().display().to_string();
    text(
        hdc,
        s.f_body,
        TXT_2,
        PAD,
        top + row_title_h,
        &ellipsize(&dir, 34),
    );

    let button_w = ui(50);
    let button_h = ui(28);
    s.dir_open = rect(
        CR - button_w,
        SAVE_CY - button_h / 2,
        CR,
        SAVE_CY + button_h / 2,
    );
    s.dir_edit = rect(
        CR - button_w * 2 - ui(8),
        SAVE_CY - button_h / 2,
        CR - button_w - ui(8),
        SAVE_CY + button_h / 2,
    );
    draw_button(hdc, &s.dir_edit, "更改", s.f_action);
    draw_button(hdc, &s.dir_open, "打开", s.f_action);

    solid(hdc, &rect(PAD, DIV_SAVE_Y, CR, DIV_SAVE_Y + 1), DIVIDER);

    let top = ASK_CY - (row_title_h + row_body_h) / 2;
    text(hdc, s.f_h1, TXT_1, PAD, top, "录完询问另存为");
    text(
        hdc,
        s.f_body,
        TXT_2,
        PAD,
        top + row_title_h,
        "否则直接存到上面的目录",
    );

    let (ask_yes, ask_no) = segment_rects(hdc, CR, ASK_CY, "是", "否", s.f_body);
    s.ask_yes = ask_yes;
    s.ask_no = ask_no;
    draw_segment_track(hdc, &ask_yes, &ask_no);
    draw_chip(hdc, &ask_yes, "是", s.cfg.video_ask_save, s.f_body);
    draw_chip(hdc, &ask_no, "否", !s.cfg.video_ask_save, s.f_body);

    solid(hdc, &rect(PAD, DIV_OPEN_Y, CR, DIV_OPEN_Y + 1), DIVIDER);

    let top = OPEN_CY - (row_title_h + row_body_h) / 2;
    text(hdc, s.f_h1, TXT_1, PAD, top, "录完打开目录");
    text(
        hdc,
        s.f_body,
        TXT_2,
        PAD,
        top + row_title_h,
        "在资源管理器里定位到刚录的文件",
    );

    let (open_yes, open_no) = segment_rects(hdc, CR, OPEN_CY, "是", "否", s.f_body);
    s.open_yes = open_yes;
    s.open_no = open_no;
    draw_segment_track(hdc, &open_yes, &open_no);
    draw_chip(hdc, &open_yes, "是", s.cfg.video_open_dir, s.f_body);
    draw_chip(hdc, &open_no, "否", !s.cfg.video_open_dir, s.f_body);

    // ---- 页脚说明 ----
    let footer = "组合键被其他程序占用时会自动还原。";
    let (_, footer_h) = measure(hdc, s.f_body, footer);
    let icon_size = ui(13);
    let icon_top = FOOTER_Y + (footer_h - icon_size) / 2;
    let icon = rect(PAD, icon_top, PAD + icon_size, icon_top + icon_size);
    crate::gdiplus::ellipse(hdc, &icon, TXT_2, HAIRLINE);
    text_at(
        hdc,
        s.f_brand,
        TXT_2,
        (icon.left + icon.right) / 2,
        (icon.top + icon.bottom) / 2,
        "i",
    );
    text(hdc, s.f_body, TXT_2, PAD + ui(18), FOOTER_Y, footer);
}

pub fn register() {
    unsafe {
        let class = to_wide(CLASS);
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
    }
}
