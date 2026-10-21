//! 设置窗口。
//!
//! 自绘面板，不用对话框资源。目前只有一项：截图热键。
//! 点一下快捷键键帽进入录制，按下组合键即生效并落盘。
//!
//! 视觉上按「Windows 设置卡片」的形态做：浅灰底、白色圆角卡片、
//! 行内左侧标签右侧控件。快捷键不画成输入框——它展示的是一个组合，
//! 不是等待文本输入，所以拆成几枚键帽更贴近它的语义。

use crate::config::{
    hotkey_parts, Config, DEFAULT_MOD, DEFAULT_VK, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN,
};
use crate::gdiplus::FontSpec;
use crate::win::{hinstance, pt_in, rect, to_wide};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect, SetBkMode, HDC,
    HGDIOBJ, PAINTSTRUCT, TRANSPARENT,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, GetClientRect, GetSystemMetrics,
    LoadCursorW, PostMessageW, RegisterClassW, SetCursor, SetForegroundWindow, ShowWindow,
    CS_HREDRAW, CS_VREDRAW, HCURSOR, HTCLIENT, IDC_ARROW, IDC_HAND, SM_CXSCREEN, SM_CYSCREEN,
    SW_SHOW, WM_APP, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_MOUSEMOVE,
    WM_PAINT, WM_SETCURSOR, WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW, WS_CAPTION, WS_EX_TOOLWINDOW,
    WS_OVERLAPPED, WS_SYSMENU,
};

const CLASS: &str = "SmartPasteSettings";

/// windows-sys 0.59 没导出这个常量
const WM_MOUSELEAVE: u32 = 0x02A3;

/// 热键改完之后通知主窗口重注册
pub const WM_HOTKEY_CHANGED: u32 = WM_APP + 3;
/// 开始录制，让主窗口先把热键注销掉
pub const WM_RECORD_START: u32 = WM_APP + 4;

// 跟 overlay 里的缩放系数保持一致
const UI_SCALE: (i32, i32) = (8, 5);
const fn ui(v: i32) -> i32 {
    v * UI_SCALE.0 / UI_SCALE.1
}

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

// ---- 色板：中性灰，只有录制态用产品玫红 ----
const BG: u32 = rgb(0xF7, 0xF7, 0xF9);
const CARD_BG: u32 = rgb(0xFF, 0xFF, 0xFF);
const CARD_BORDER: u32 = rgb(0xE6, 0xE6, 0xEC);
const DIVIDER: u32 = rgb(0xEF, 0xEF, 0xF3);

const TXT_1: u32 = rgb(0x1A, 0x1A, 0x20); // 主文字
const TXT_2: u32 = rgb(0x6E, 0x6E, 0x78); // 次文字
const TXT_3: u32 = rgb(0x9A, 0x9A, 0xA4); // 辅助文字
const TXT_LABEL: u32 = rgb(0x85, 0x85, 0x8F); // 分区标签
const TXT_OFF: u32 = rgb(0xC2, 0xC2, 0xCB); // 不可用

const ACCENT: u32 = rgb(0xFF, 0x2E, 0x63);
const ACCENT_BG: u32 = rgb(0xFF, 0xF2, 0xF6);

const KEY_BG: u32 = rgb(0xFB, 0xFB, 0xFD);
const KEY_BG_HOVER: u32 = rgb(0xF2, 0xF2, 0xF6);
const KEY_BORDER: u32 = rgb(0xDE, 0xDE, 0xE5);
const KEY_BORDER_HOVER: u32 = rgb(0xC9, 0xC9, 0xD4);
const KEY_TXT: u32 = rgb(0x33, 0x33, 0x3C);
const PLUS: u32 = rgb(0xA6, 0xA6, 0xB0);
const HOVER_BG: u32 = rgb(0xF2, 0xF2, 0xF5);

/// 描边宽度要跟着 1.6 倍缩放走。
/// 写死 1px 的话，在 16px 半径的圆角上会断成一节一节，看着发毛。
const HAIRLINE: f32 = 1.6;

// ---- 布局：左右 24 外边距，行高按 8 的倍数走 ----
const W: i32 = ui(480);
const H: i32 = ui(250);

const PAD: i32 = ui(24);
const CARD: RECT = rect(PAD, ui(51), W - PAD, ui(194));
const CX: i32 = PAD + ui(20); // 卡片内容左边界
const CR: i32 = W - PAD - ui(20); // 卡片内容右边界

const ROW1_CY: i32 = ui(95);
const DIVIDER_Y: i32 = ui(138);
const ROW2_CY: i32 = ui(166);

const KEY_H: i32 = ui(32);
const KEY_GAP: i32 = ui(22);
const KEY_PAD_X: i32 = ui(10);

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
    tracking: bool,
    hot_rect: RECT,
    reset_rect: RECT,
    hand: HCURSOR,

    f_h1: FontSpec,
    f_body: FontSpec,
    f_key: FontSpec,
    f_plus: FontSpec,
    f_section: FontSpec,
    f_link: FontSpec,
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

        // W/H 是客户区尺寸，CreateWindowExW 要的是含标题栏和边框的外框尺寸，
        // 直接传客户区高度会把底部的说明文字裁掉
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
        let mut frame = rect(0, 0, W, H);
        AdjustWindowRectEx(&mut frame, style, 0, WS_EX_TOOLWINDOW);
        let (ww, wh) = (frame.right - frame.left, frame.bottom - frame.top);

        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let x = (sw - ww) / 2;
        let y = (sh - wh) / 2;

        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.as_ptr(),
            title.as_ptr(),
            style,
            x,
            y,
            ww,
            wh,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance(),
            std::ptr::null_mut(),
        );
        if hwnd.is_null() {
            crate::win::trace("settings: CreateWindow failed");
            return;
        }

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
            tracking: false,
            hot_rect: rect(0, 0, 0, 0),
            reset_rect: rect(0, 0, 0, 0),
            hand: LoadCursorW(std::ptr::null_mut(), IDC_HAND),

            f_h1: FontSpec::new(ui(15) as f32, true, "Microsoft YaHei"),
            f_body: FontSpec::new(ui(12) as f32, false, "Microsoft YaHei"),
            f_key: FontSpec::new(ui(14) as f32, true, "Segoe UI"),
            f_plus: FontSpec::new(ui(14) as f32, false, "Segoe UI"),
            f_section: FontSpec::new(ui(11) as f32, true, "Microsoft YaHei"),
            f_link: FontSpec::new(ui(13) as f32, false, "Microsoft YaHei"),
            f_action: FontSpec::new(ui(13) as f32, true, "Microsoft YaHei"),
        }));

        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        SetFocus(hwnd);
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
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
                if hot != s.hover_hot || reset != s.hover_reset {
                    s.hover_hot = hot;
                    s.hover_reset = reset;
                    // WM_SETCURSOR 会在鼠标停下时不再来，这里直接设定
                    if hot || reset {
                        SetCursor(s.hand);
                    }
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                if (hot || reset) && !s.tracking {
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
                if s.hover_hot || s.hover_reset {
                    s.hover_hot = false;
                    s.hover_reset = false;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                0
            }
            WM_SETCURSOR => {
                let hit = (lparam as u32) & 0xFFFF;
                if hit == HTCLIENT && (s.hover_hot || s.hover_reset) {
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

                if pt_in(&s.hot_rect, x, y) {
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
                } else if !s.recording && !is_default(&s.cfg) && pt_in(&s.reset_rect, x, y) {
                    s.cfg = Config::default();
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
                if s.recording {
                    let vk = wparam as u32;
                    // 修饰键状态在每次按键都刷新，哪怕主键的 down 被吞也有记录
                    s.live_mod = modifiers();
                    if !is_modifier(vk) {
                        s.pend_vk = vk;
                        // 修饰键要在按下这一刻取，等到 KEYUP 时可能已经松了
                        s.pend_mod = s.live_mod;
                    }
                    InvalidateRect(hwnd, std::ptr::null(), 0);
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

/// 键帽组，右对齐到卡片内容右边界；返回用于点击判定的外扩矩形。
///
/// `avail` 是右侧能用的宽度。Ctrl+Shift+Alt+A 这种四键组合比三键宽 60px，
/// 会和左边的说明文字撞上，所以装不下时先把内边距和间距各收一档。
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

    let top = ROW1_CY - KEY_H / 2;
    let (bg, border) = if s.hover_hot {
        (KEY_BG_HOVER, KEY_BORDER_HOVER)
    } else {
        (KEY_BG, KEY_BORDER)
    };

    let mut x = CR - total;
    for (i, p) in parts.iter().enumerate() {
        let r = rect(x, top, x + widths[i], top + KEY_H);
        crate::gdiplus::fill_round(dc, &r, ui(7) as f32, bg);
        crate::gdiplus::stroke_round(dc, &r, ui(7) as f32, border, HAIRLINE);
        text_at(dc, s.f_key, KEY_TXT, (r.left + r.right) / 2, ROW1_CY, p);

        x += widths[i];
        if i + 1 < parts.len() {
            text_at(dc, s.f_plus, PLUS, x + gap / 2, ROW1_CY, "+");
            x += gap;
        }
    }

    expand(&rect(CR - total, top, CR, top + KEY_H), ui(8))
}

unsafe fn paint(s: &mut State) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(s.hwnd, &mut ps);

    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(s.hwnd, &mut rc);
    solid(hdc, &rc, BG);
    SetBkMode(hdc, TRANSPARENT as i32);

    // 分区标签：字号更小、颜色更浅，和正文拉开层级
    text(hdc, s.f_section, TXT_LABEL, PAD, ui(24), "快捷键");

    // 卡片
    crate::gdiplus::fill_round(hdc, &CARD, ui(10) as f32, CARD_BG);
    crate::gdiplus::stroke_round(hdc, &CARD, ui(10) as f32, CARD_BORDER, HAIRLINE);

    // 第一行左侧：标题 + 说明，两行作为一块垂直居中
    let (_, h1) = measure(hdc, s.f_h1, "截图快捷键");
    let (_, h2) = measure(hdc, s.f_body, "点击按键框");
    // GDI+ 的行高里已经含了下沉部分，再额外加间距会显得两行离得远
    let gap = 0;
    let top = ROW1_CY - (h1 + gap + h2) / 2;
    text(hdc, s.f_h1, TXT_1, CX, top, "截图快捷键");

    let (desc, desc_color) = if s.recording {
        ("按下组合键，Esc 取消", ACCENT)
    } else {
        ("点击按键框，按下新的组合键", TXT_2)
    };
    text(hdc, s.f_body, desc_color, CX, top + h1 + gap, desc);

    // 右侧控件的可用宽度：扣掉左边两行文字和 24px 的安全间距
    let text_w = measure(hdc, s.f_h1, "截图快捷键")
        .0
        .max(measure(hdc, s.f_body, "点击按键框，按下新的组合键").0);
    let avail = CR - CX - text_w - ui(24);

    // 第一行右侧：键帽，或录制中的胶囊
    if s.recording {
        let pill = rect(CR - ui(196), ROW1_CY - ui(17), CR, ROW1_CY + ui(17));
        crate::gdiplus::fill_round(hdc, &pill, ui(9) as f32, ACCENT_BG);
        crate::gdiplus::stroke_round(hdc, &pill, ui(9) as f32, ACCENT, ui(2) as f32);
        text_at(
            hdc,
            s.f_action,
            ACCENT,
            (pill.left + pill.right) / 2,
            ROW1_CY,
            "按下组合键…",
        );
        s.hot_rect = expand(&pill, ui(8));
    } else {
        s.hot_rect = draw_keycaps(s, hdc, avail);
    }

    // 分隔线：内缩，不顶到卡片边
    solid(hdc, &rect(CX, DIVIDER_Y, CR, DIVIDER_Y + 1), DIVIDER);

    // 第二行：恢复默认
    let (_, lh) = measure(hdc, s.f_link, "恢复默认快捷键");
    text(hdc, s.f_link, TXT_2, CX, ROW2_CY - lh / 2, "恢复默认快捷键");

    let default = is_default(&s.cfg);
    let label = "恢复";
    let (tw, _) = measure(hdc, s.f_action, label);
    let bw = tw + ui(18);
    let bh = ui(28);
    let btn = rect(CR - bw, ROW2_CY - bh / 2, CR, ROW2_CY + bh / 2);
    s.reset_rect = btn;

    if !default && s.hover_reset {
        crate::gdiplus::fill_round(hdc, &btn, ui(7) as f32, HOVER_BG);
    }
    let color = if default {
        TXT_OFF
    } else if s.hover_reset {
        ACCENT
    } else {
        TXT_2
    };
    text_at(hdc, s.f_action, color, CR - bw / 2, ROW2_CY, label);

    // 页脚说明
    text(
        hdc,
        s.f_body,
        TXT_3,
        PAD,
        ui(210),
        "组合键被其他程序占用时，会自动还原为原来的设置。",
    );

    EndPaint(s.hwnd, &ps);
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
