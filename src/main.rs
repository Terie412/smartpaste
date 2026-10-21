#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(static_mut_refs)]

mod capture;
mod clipboard;
mod edit;
mod gdiplus;
mod icon;
mod icons;
mod overlay;
mod pin;
mod win;

use std::path::PathBuf;
use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, POINT, WPARAM, ERROR_ALREADY_EXISTS};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, PostQuitMessage, RegisterClassW,
    SetForegroundWindow, TrackPopupMenu, TranslateMessage, HICON, HWND_MESSAGE, MF_STRING, MSG,
    SM_CXSMICON, TPM_RIGHTBUTTON, WM_APP, WM_COMMAND, WM_DESTROY, WM_HOTKEY, WM_LBUTTONUP,
    WM_RBUTTONUP, WNDCLASSW,
};

use overlay::{CMD_CANCEL, CMD_COPY, CMD_PIN, CMD_SAVE, WM_SNAP_DONE};
use win::{hinstance, to_wide};

const WM_TRAY: u32 = WM_APP + 1;
const HOTKEY_ID: i32 = 0xA1;
const IDM_SNAP: usize = 1001;
const IDM_EXIT: usize = 1002;

fn main() {
    gdiplus::startup();

    unsafe {
        use windows_sys::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    if !ensure_single_instance() {
        return;
    }

    // 数据链路自检：不依赖任何交互，验证 抓屏 -> 裁剪 -> DIB -> 剪贴板 -> PNG
    if std::env::args().any(|a| a == "--selftest") {
        let Some(screen) = capture::capture_desktop() else {
            win::trace("selftest: capture FAILED");
            return;
        };
        let Some(img) = screen.crop_rgba(100, 100, 200, 100) else {
            win::trace("selftest: crop FAILED");
            return;
        };
        let clip = clipboard::copy_image(&img);
        let png = save_png(&img);
        win::trace(&format!(
            "selftest: screen {}x{} crop {}x{} clip={:?} png={:?}",
            screen.width,
            screen.height,
            img.width(),
            img.height(),
            clip.is_ok(),
            png.map(|p| p.display().to_string())
        ));
        return;
    }

    // 调试入口：--rect x y w h 让遮罩直接锁定一块区域，跳过鼠标拖拽。
    // 模拟鼠标在 DPI 缩放下坐标不可靠，调样式时用它截图更稳。
    let argv: Vec<String> = std::env::args().collect();
    win::trace(&format!("args: {argv:?}"));
    if let Some(i) = argv.iter().position(|a| a == "--rect") {
        let v: Vec<i32> = argv
            .iter()
            .skip(i + 1)
            .take(4)
            .filter_map(|s| s.parse().ok())
            .collect();
        win::trace(&format!("rect parsed: {v:?}"));
        if v.len() == 4 {
            overlay::set_preset(v[0], v[1], v[2], v[3]);
        }
    }

    overlay::register();
    pin::register();

    unsafe {
        let class = to_wide("SmartPasteMain");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(main_wndproc),
            hInstance: hinstance(),
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            to_wide("SmartPaste").as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            hinstance(),
            std::ptr::null_mut(),
        );
        if hwnd.is_null() {
            return;
        }
        overlay::set_main_hwnd(hwnd);
        add_tray(hwnd);

        // Ctrl+Alt+X：注册到线程而非窗口。HWND_MESSAGE 窗口收不到 WM_HOTKEY，
        // 线程级热键会直接进本线程消息队列，循环里截获最稳。
        let hk = RegisterHotKey(std::ptr::null_mut(), HOTKEY_ID, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, 0x58);
        win::trace(&format!(
            "started: hwnd={:?} hotkey_registered={}",
            hwnd,
            hk != 0
        ));

        let mut msg: MSG = std::mem::zeroed();

        // 调试入口：--auto 让程序 1.5s 后自己投递一次热键消息，
        // 用来验证消息链路（模拟按键在某些环境里注入不进去）。
        if std::env::args().any(|a| a == "--auto") {
            use windows_sys::Win32::System::Threading::GetCurrentThreadId;
            use windows_sys::Win32::UI::WindowsAndMessaging::PostThreadMessageW;
            let tid = GetCurrentThreadId();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(1500));
                win::trace("auto: posting synthetic WM_HOTKEY");
                PostThreadMessageW(tid, WM_HOTKEY, HOTKEY_ID as usize, 0);
            });
        }

        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            if msg.message == WM_HOTKEY && msg.wParam as i32 == HOTKEY_ID {
                win::trace("WM_HOTKEY received");
                let ok = overlay::start();
                win::trace(&format!("overlay::start -> {ok}"));
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
            pin::reap();
        }

        remove_tray(hwnd);
    }
}

fn ensure_single_instance() -> bool {
    let name = to_wide("Global\\SmartPaste.SingleInstance");
    unsafe {
        let h = CreateMutexW(std::ptr::null_mut(), 1, name.as_ptr());
        if h.is_null() {
            return true;
        }
        GetLastError() != ERROR_ALREADY_EXISTS
    }
}

/// 托盘图标句柄。Shell_NotifyIcon 之后仍要保留，退出时销毁。
static mut TRAY_ICON: HICON = std::ptr::null_mut();

unsafe fn add_tray(hwnd: HWND) {
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    // 托盘按 DPI 用 16/20/24，正好对上 ICO 里简化过的那几帧
    let metric = GetSystemMetrics(SM_CXSMICON);
    nid.hIcon = icon::for_size(metric);
    TRAY_ICON = nid.hIcon;
    let added = Shell_NotifyIconW(NIM_ADD, &nid);
    win::trace(&format!(
        "tray: icon={:?} size={}x{} add={}",
        nid.hIcon, metric, metric, added
    ));
    for (i, c) in "SmartPaste — Ctrl+Alt+X 截图".encode_utf16().enumerate() {
        if i < nid.szTip.len() {
            nid.szTip[i] = c;
        }
    }
    Shell_NotifyIconW(NIM_ADD, &nid);
}

unsafe fn remove_tray(hwnd: HWND) {
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    Shell_NotifyIconW(NIM_DELETE, &nid);
    icon::destroy(TRAY_ICON);
    TRAY_ICON = std::ptr::null_mut();
}

unsafe fn show_menu(hwnd: HWND) {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }
    AppendMenuW(menu, MF_STRING, IDM_SNAP, to_wide("截图  Ctrl+Alt+X").as_ptr());
    AppendMenuW(menu, MF_STRING, IDM_EXIT, to_wide("退出").as_ptr());

    let mut pt: POINT = std::mem::zeroed();
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd);
    TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, std::ptr::null());
    DestroyMenu(menu);
}

extern "system" fn main_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_HOTKEY => {
                win::trace("WM_HOTKEY received");
                if wparam as i32 == HOTKEY_ID {
                    let ok = overlay::start();
                    win::trace(&format!("overlay::start -> {ok}"));
                }
                0
            }
            WM_TRAY => {
                let ev = lparam as u32;
                if ev == WM_RBUTTONUP || ev == WM_LBUTTONUP {
                    show_menu(hwnd);
                }
                0
            }
            WM_COMMAND => {
                match wparam & 0xFFFF {
                    IDM_SNAP => {
                        overlay::start();
                    }
                    IDM_EXIT => {
                        DestroyWindow(hwnd);
                    }
                    _ => {}
                }
                0
            }
            WM_SNAP_DONE => {
                overlay::close();
                let (cmd, img, origin) = overlay::take_pending();
                handle_command(cmd, img, origin);
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

fn handle_command(cmd: u32, img: Option<image::RgbaImage>, origin: POINT) {
    let Some(img) = img else {
        win::trace(&format!("cmd {cmd}: no image"));
        return;
    };
    win::trace(&format!(
        "cmd {cmd}: {}x{} at ({},{})",
        img.width(),
        img.height(),
        origin.x,
        origin.y
    ));
    match cmd {
        CMD_COPY => match clipboard::copy_image(&img) {
            Ok(()) => win::trace("copy ok"),
            Err(e) => win::trace(&format!("copy failed: {e}")),
        },
        CMD_SAVE => match save_png(&img) {
            Some(p) => win::trace(&format!("saved {}", p.display())),
            None => win::trace("save failed"),
        },
        CMD_PIN => {
            win::trace(if pin::create(&img, origin.x, origin.y) {
                "pin created"
            } else {
                "pin failed"
            });
        }
        CMD_CANCEL => {}
        _ => {}
    }
}

fn save_png(img: &image::RgbaImage) -> Option<PathBuf> {
    let base = std::env::var_os("USERPROFILE").map(PathBuf::from)?;
    let mut dir = base.join("Pictures");
    if !dir.exists() {
        dir = base.join("Desktop");
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("SmartPaste_{ts}.png"));
    img.save(&path).ok()?;
    Some(path)
}
