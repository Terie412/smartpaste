//! 设置窗口。
//!
//! 自绘的轻量面板，不引对话框资源。目前只有一项：截图热键。
//! 点一下热键框进入录制状态，按下组合键就生效并落盘。

use crate::config::{Config, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN};
use crate::win::{delete_font, hinstance, pt_in, rect, to_wide, ui_font};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect, SelectObject,
    SetBkMode, SetTextColor, TextOutW, HFONT, HGDIOBJ, PAINTSTRUCT, TRANSPARENT,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetClientRect, GetSystemMetrics,
    LoadCursorW, PostMessageW, RegisterClassW, SetForegroundWindow, ShowWindow, CS_HREDRAW,
    CS_VREDRAW, IDC_ARROW, SM_CXSCREEN, SM_CYSCREEN, SW_SHOW, WM_APP,
    WM_DESTROY, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_PAINT, WM_SYSKEYDOWN, WM_SYSKEYUP,
    WNDCLASSW, WS_CAPTION,
    WS_EX_TOOLWINDOW, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU,
};

const CLASS: &str = "SmartPasteSettings";

/// 热键改完之后通知主窗口重注册
pub const WM_HOTKEY_CHANGED: u32 = WM_APP + 3;
/// 开始录制，让主窗口先把热键注销掉
pub const WM_RECORD_START: u32 = WM_APP + 4;

// 跟 overlay 里的缩放系数保持一致
const UI_SCALE: (i32, i32) = (8, 5);
const fn ui(v: i32) -> i32 {
    v * UI_SCALE.0 / UI_SCALE.1
}

const W: i32 = ui(420);
const H: i32 = ui(190);

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
    hot_rect: RECT,
    font: HFONT,
    small: HFONT,
}

static mut S: Option<Box<State>> = None;

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

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

        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
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

        let pad = ui(24);
        S = Some(Box::new(State {
            hwnd,
            parent,
            cfg: Config::load(),
            recording: false,
            pend_vk: 0,
            pend_mod: 0,
            live_mod: 0,
            hot_rect: rect(pad, ui(42), W - pad, ui(42) + ui(52)),
            font: ui_font(ui(13)),
            small: ui_font(ui(11)),
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
            WM_LBUTTONDOWN => {
                let (x, y) = (
                    (lparam as usize & 0xFFFF) as u16 as i16 as i32,
                    ((lparam as usize >> 16) & 0xFFFF) as u16 as i16 as i32,
                );
                // 点击就是「用户输入」，这时抢前台才被系统允许，
                // 否则窗口只在后台，键盘消息根本进不来
                SetForegroundWindow(hwnd);
                if pt_in(&s.hot_rect, x, y) {
                    s.recording = true;
                    s.pend_vk = 0;
                    s.pend_mod = 0;
                    s.live_mod = 0;
                    SetFocus(hwnd);
                    // 录制期间必须先把热键摘掉，否则按到当前生效的组合
                    // 会直接被系统拿去触发截图，窗口收不到按键
                    if !s.parent.is_null() {
                        PostMessageW(s.parent, WM_RECORD_START, 0, 0);
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
                        let pm = if s.pend_mod != 0 { s.pend_mod } else { s.live_mod };
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
                if let Some(st) = S.take() {
                    delete_font(st.font);
                    delete_font(st.small);
                }
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// 落盘 + 通知主窗口重注册热键
unsafe fn apply_hotkey(s: &mut State, vk: u32, mods: u32) {
    s.cfg.hotkey_mod = mods;
    s.cfg.hotkey_vk = vk;
    s.cfg.save();
    crate::win::trace(&format!("settings: hotkey -> {}", s.cfg.hotkey_text()));
}

unsafe fn paint(s: &mut State) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(s.hwnd, &mut ps);

    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(s.hwnd, &mut rc);

    // 白底，跟遮罩上的浮层保持一致
    let bg = CreateSolidBrush(rgb(0xFF, 0xFF, 0xFF));
    FillRect(hdc, &rc, bg);
    DeleteObject(bg as HGDIOBJ);

    SetBkMode(hdc, TRANSPARENT as i32);
    let pad = ui(24);

    // 标题行
    let label = to_wide("截图快捷键");
    let old = SelectObject(hdc, s.font as HGDIOBJ);
    SetTextColor(hdc, rgb(0x2A, 0x2A, 0x30));
    TextOutW(hdc, pad, ui(18), label.as_ptr(), (label.len() - 1) as i32);
    SelectObject(hdc, old);

    // 热键框：录制中换成玫红边框 + 提示语
    let fill = if s.recording { 0x00F5_F0FF } else { 0x00F7_F5F5 };
    let border = if s.recording { 0x0063_2EFF } else { 0x00DC_D6D6 };
    crate::gdiplus::fill_round(hdc, &s.hot_rect, ui(10) as f32, fill);
    crate::gdiplus::stroke_round(hdc, &s.hot_rect, ui(10) as f32, border, ui(2) as f32);

    let text = if s.recording {
        "按下新的组合键…".to_string()
    } else {
        s.cfg.hotkey_text()
    };
    let wide = to_wide(&text);
    let old = SelectObject(hdc, s.font as HGDIOBJ);
    SetTextColor(
        hdc,
        if s.recording {
            rgb(0xFF, 0x2E, 0x63)
        } else {
            rgb(0x1A, 0x1A, 0x20)
        },
    );
    TextOutW(
        hdc,
        s.hot_rect.left + ui(18),
        s.hot_rect.top + ui(16),
        wide.as_ptr(),
        (wide.len() - 1) as i32,
    );
    SelectObject(hdc, old);

    // 说明
    let hint = to_wide("点击上面的框，然后按下新的组合键。改完立即生效。");
    let old = SelectObject(hdc, s.small as HGDIOBJ);
    SetTextColor(hdc, rgb(0x8A, 0x8A, 0x92));
    TextOutW(
        hdc,
        pad,
        s.hot_rect.bottom + ui(14),
        hint.as_ptr(),
        (hint.len() - 1) as i32,
    );
    SelectObject(hdc, old);

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
