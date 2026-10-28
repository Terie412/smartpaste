#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(static_mut_refs)]

mod capture;
mod clipboard;
mod config;
mod edit;
mod explorer;
mod gdiplus;
mod icon;
mod icons;
mod overlay;
mod pin;
mod record;
mod savedlg;
mod settings;
mod win;

use std::path::PathBuf;
use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, MOD_NOREPEAT,
};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_ERROR, NIIF_INFO, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, KillTimer, MessageBoxW,
    PostQuitMessage, RegisterClassW, SetForegroundWindow, SetTimer, TrackPopupMenu,
    TranslateMessage, HICON, HWND_MESSAGE, MB_ICONWARNING, MB_OK, MF_STRING, MSG, SM_CXSMICON,
    TPM_RIGHTBUTTON, WM_APP, WM_COMMAND, WM_DESTROY, WM_HOTKEY, WM_LBUTTONUP, WM_RBUTTONUP,
    WM_TIMER, WNDCLASSW,
};

use overlay::{CMD_CANCEL, CMD_COPY, CMD_PIN, CMD_RECORD, CMD_SAVE, WM_SNAP_DONE};
use record::session;
use win::{hinstance, to_wide};

const WM_TRAY: u32 = WM_APP + 1;
const HOTKEY_ID: i32 = 0xA1;
const IDM_SNAP: usize = 1001;
const IDM_EXIT: usize = 1002;
const IDM_SETTINGS: usize = 1003;
const IDM_RECORD: usize = 1004;
/// 录制中每秒刷一次托盘提示
const TIMER_REC: usize = 0x71;

/// 当前真正注册成功的热键，用来在改键失败时回滚
static mut ACTIVE_HOTKEY: (u32, u32) = (0, 0);

fn main() {
    gdiplus::startup();

    unsafe {
        use windows_sys::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // 录屏自测：不碰 UI，直接录一块区域，验证 DDA + 硬编的真实帧率和 CPU。
    // smartpaste --rectest x y w h seconds [path] [fps]
    // 放在单实例检查之前，免得托盘版在跑时静默退出。
    if std::env::args().any(|a| a == "--rectest") {
        run_rectest();
        return;
    }

    // 录制会话自测：走完整的"开录 → 红框 → 线程 → 停止"链路，到点自动停。
    // smartpaste --recnow x y w h seconds [fps]
    if std::env::args().any(|a| a == "--recnow") {
        run_recnow();
        return;
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
    settings::register();
    session::register();

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

        // 调试/快捷入口：启动就把设置窗口拉出来
        if std::env::args().any(|a| a == "--settings") {
            settings::open(hwnd);
        }

        // 热键注册到线程而非窗口：HWND_MESSAGE 窗口收不到 WM_HOTKEY，
        // 线程级热键会直接进本线程消息队列，循环里截获最稳。
        let cfg = config::Config::load();
        let hk = RegisterHotKey(
            std::ptr::null_mut(),
            HOTKEY_ID,
            cfg.hotkey_mod | MOD_NOREPEAT,
            cfg.hotkey_vk,
        );
        if hk != 0 {
            ACTIVE_HOTKEY = (cfg.hotkey_mod, cfg.hotkey_vk);
        }
        win::trace(&format!(
            "started: hwnd={:?} hotkey={} registered={}",
            hwnd,
            cfg.hotkey_text(),
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
                if session::is_recording() {
                    finish_recording(hwnd);
                } else {
                    let ok = overlay::start();
                    win::trace(&format!("overlay::start -> {ok}"));
                }
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
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // 以前是静默退出，结果"更新了程序但跑的还是旧进程"，很难查。
            // 这里明确告诉用户：托盘里那个实例才是正在跑的代码。
            let text = to_wide(
                "SmartPaste 已经在运行，图标在系统托盘里。\n\n\
                 如果刚刚更新了程序，请先在托盘图标上右键 → 退出，再重新启动。",
            );
            let caption = to_wide("SmartPaste");
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                caption.as_ptr(),
                MB_OK | MB_ICONWARNING,
            );
            return false;
        }
        true
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
    let tip = format!("SmartPaste — {} 截图", config::Config::load().hotkey_text());
    for (i, c) in tip.encode_utf16().enumerate() {
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
    let cfg = config::Config::load();
    if session::is_recording() {
        let s = session::elapsed_secs();
        let label = to_wide(&format!("停止录制  {:02}:{:02}", s / 60, s % 60));
        AppendMenuW(menu, MF_STRING, IDM_RECORD, label.as_ptr());
    } else {
        let snap_label = to_wide(&format!("截图  {}", cfg.hotkey_text()));
        AppendMenuW(menu, MF_STRING, IDM_SNAP, snap_label.as_ptr());
        AppendMenuW(menu, MF_STRING, IDM_RECORD, to_wide("录屏…").as_ptr());
    }
    AppendMenuW(menu, MF_STRING, IDM_SETTINGS, to_wide("设置…").as_ptr());
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
                    if session::is_recording() {
                        finish_recording(hwnd);
                    } else {
                        let ok = overlay::start();
                        win::trace(&format!("overlay::start -> {ok}"));
                    }
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
                    IDM_RECORD => {
                        if session::is_recording() {
                            finish_recording(hwnd);
                        } else {
                            overlay::start();
                        }
                    }
                    IDM_SETTINGS => {
                        settings::open(hwnd);
                    }
                    IDM_EXIT => {
                        DestroyWindow(hwnd);
                    }
                    _ => {}
                }
                0
            }
            settings::WM_RECORD_START => {
                UnregisterHotKey(std::ptr::null_mut(), HOTKEY_ID);
                win::trace("hotkey: 录制中，暂时注销");
                0
            }
            settings::WM_HOTKEY_CHANGED => {
                let cfg = config::Config::load();
                let (old_mod, old_vk) = ACTIVE_HOTKEY;
                UnregisterHotKey(std::ptr::null_mut(), HOTKEY_ID);

                let ok = RegisterHotKey(
                    std::ptr::null_mut(),
                    HOTKEY_ID,
                    cfg.hotkey_mod | MOD_NOREPEAT,
                    cfg.hotkey_vk,
                );

                if ok == 0 {
                    // 目标组合被别的程序占了：把旧热键装回去，配置也回滚，
                    // 否则程序会变成一个没有任何热键的哑巴
                    let back = RegisterHotKey(
                        std::ptr::null_mut(),
                        HOTKEY_ID,
                        old_mod | MOD_NOREPEAT,
                        old_vk,
                    );
                    config::Config {
                        hotkey_mod: old_mod,
                        hotkey_vk: old_vk,
                        styles: cfg.styles.clone(),
                        save_dir: cfg.save_dir.clone(),
                        record_fps: cfg.record_fps,
                        record_countdown: cfg.record_countdown,
                        video_dir: cfg.video_dir.clone(),
                        video_ask_save: cfg.video_ask_save,
                        video_open_dir: cfg.video_open_dir,
                        recent_colors: cfg.recent_colors.clone(),
                    }
                    .save();
                    win::trace(&format!(
                        "hotkey: {} 注册失败，回滚到 {} (back={})",
                        cfg.hotkey_text(),
                        config::Config {
                            hotkey_mod: old_mod,
                            styles: cfg.styles.clone(),
                            save_dir: cfg.save_dir.clone(),
                            hotkey_vk: old_vk,
                            record_fps: cfg.record_fps,
                            record_countdown: cfg.record_countdown,
                            video_dir: cfg.video_dir.clone(),
                            video_ask_save: cfg.video_ask_save,
                            video_open_dir: cfg.video_open_dir,
                            recent_colors: cfg.recent_colors.clone(),
                        }
                        .hotkey_text(),
                        back
                    ));
                    MessageBoxW(
                        hwnd,
                        to_wide("这个组合键已被其他程序占用，已恢复原来的快捷键。").as_ptr(),
                        to_wide("smartpaste").as_ptr(),
                        MB_OK | MB_ICONWARNING,
                    );
                } else {
                    ACTIVE_HOTKEY = (cfg.hotkey_mod, cfg.hotkey_vk);
                    win::trace(&format!("hotkey re-registered: {} ok=1", cfg.hotkey_text()));
                }
                0
            }
            WM_SNAP_DONE => {
                overlay::close();
                let (cmd, img, origin) = overlay::take_pending();
                handle_command(hwnd, cmd, img, origin);
                0
            }
            WM_TIMER if wparam == TIMER_REC => {
                if let Some(out) = session::poll() {
                    KillTimer(hwnd, TIMER_REC);
                    update_tray(hwnd);
                    report(hwnd, out);
                } else {
                    session::refresh();
                    update_tray(hwnd);
                }
                0
            }
            session::WM_REC_STOP => {
                finish_recording(hwnd);
                0
            }
            WM_DESTROY => {
                if session::is_recording() {
                    let _ = session::stop();
                }
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

fn handle_command(hwnd: HWND, cmd: u32, img: Option<image::RgbaImage>, origin: POINT) {
    if cmd == CMD_RECORD {
        let rect = overlay::take_rect();
        let cfg = config::Config::load();
        match session::start(hwnd, rect, &cfg) {
            Ok(()) => {
                unsafe {
                    SetTimer(hwnd, TIMER_REC, 1000, None);
                }
                update_tray(hwnd);
            }
            Err(e) => {
                win::trace(&format!("record start failed: {e}"));
                toast(hwnd, "无法开始录制", &e, true);
            }
        }
        return;
    }

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
        // 保存已经在遮罩里做完（另存为对话框），这里不用再处理
        CMD_SAVE => {}
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

fn run_rectest() {
    let argv: Vec<String> = std::env::args().collect();
    let Some(i) = argv.iter().position(|a| a == "--rectest") else {
        return;
    };
    let nums: Vec<i64> = argv
        .iter()
        .skip(i + 1)
        .take(4)
        .filter_map(|s| s.parse().ok())
        .collect();
    if nums.len() < 4 {
        win::trace("rectest: 用法 --rectest x y w h [seconds] [path] [fps]");
        return;
    }
    let secs = argv
        .get(i + 5)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(5.0);
    let path = argv.get(i + 6).cloned().unwrap_or_else(|| {
        std::env::temp_dir()
            .join("smartpaste_rectest.mp4")
            .display()
            .to_string()
    });
    let fps = argv
        .get(i + 7)
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(60);

    win::trace(&format!(
        "rectest: {}x{} at ({},{}) {}s {}fps -> {path}",
        nums[2], nums[3], nums[0], nums[1], secs, fps
    ));
    match record::rectest(
        nums[0] as i32,
        nums[1] as i32,
        nums[2] as u32,
        nums[3] as u32,
        secs,
        &path,
        fps,
    ) {
        Ok(r) => win::trace(&r.summary()),
        Err(e) => win::trace(&format!("rectest FAILED: {e:?}")),
    }
}

/// 把字符串写进定长 UTF-16 数组（末尾留 0，数组本来已清零）
fn fill_wide(dst: &mut [u16], s: &str) {
    for (i, c) in s.encode_utf16().enumerate() {
        if i + 1 >= dst.len() {
            break;
        }
        dst[i] = c;
    }
}

fn update_tray(hwnd: HWND) {
    unsafe {
    let cfg = config::Config::load();
    let tip = if session::is_recording() {
        let s = session::elapsed_secs();
        format!(
            "SmartPaste — 录制中 {:02}:{:02}  ·  {} 停止",
            s / 60,
            s % 60,
            cfg.hotkey_text()
        )
    } else {
        format!("SmartPaste — {} 截图", cfg.hotkey_text())
    };

    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_TIP;
    fill_wide(&mut nid.szTip, &tip);
    Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

fn toast(hwnd: HWND, title: &str, msg: &str, error: bool) {
    unsafe {
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = if error { NIIF_ERROR } else { NIIF_INFO };
    fill_wide(&mut nid.szInfoTitle, title);
    fill_wide(&mut nid.szInfo, msg);
    Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

fn report(hwnd: HWND, out: session::Outcome) {
    match out {
        session::Outcome::Ok {
            report: r,
            ask_save,
        } => {
            let secs = r.frames as f64 / r.fps as f64;
            let note = if r.hardware { "" } else { "（软件编码）" };

            if ask_save {
                let cfg = config::Config::load();
                let dir = cfg.video_dir().display().to_string();
                let name = crate::savedlg::video_name();
                let Some(dest) = crate::savedlg::ask_video(hwnd, Some(&dir), &name) else {
                    let _ = std::fs::remove_file(&r.path);
                    toast(hwnd, "已取消", "录像没有保存", false);
                    return;
                };
                // 同盘直接 rename，跨盘再退化成 copy
                let moved = std::fs::rename(&r.path, &dest).is_ok()
                    || std::fs::copy(&r.path, &dest).is_ok();
                if moved {
                    let _ = std::fs::remove_file(&r.path);
                    crate::win::trace(&format!("record: saved to {}", dest.display()));
                    toast(
                        hwnd,
                        "录制完成",
                        &format!("已保存到 {}", dest.display()),
                        false,
                    );
                    if cfg.video_open_dir {
                        crate::explorer::reveal(&dest.display().to_string());
                    }
                } else {
                    toast(hwnd, "保存失败", "无法写入所选位置", true);
                }
                return;
            }

            if config::Config::load().video_open_dir {
                crate::explorer::reveal(&r.path);
            }
            let name = std::path::Path::new(&r.path)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            toast(
                hwnd,
                "录制完成",
                &format!("{name}  ·  {secs:.1} 秒{note}"),
                false,
            );
        }
        session::Outcome::TooShort => toast(hwnd, "录制取消", "不到 1 秒，文件已丢弃", false),
        session::Outcome::Err(e) => toast(hwnd, "录制失败", &e, true),
    }
}

fn finish_recording(hwnd: HWND) {
    if let Some(out) = session::stop() {
        unsafe {
            KillTimer(hwnd, TIMER_REC);
        }
        update_tray(hwnd);
        report(hwnd, out);
    }
}

fn run_recnow() {
    let argv: Vec<String> = std::env::args().collect();
    let Some(i) = argv.iter().position(|a| a == "--recnow") else {
        return;
    };
    let nums: Vec<i64> = argv
        .iter()
        .skip(i + 1)
        .take(4)
        .filter_map(|s| s.parse().ok())
        .collect();
    if nums.len() < 4 {
        win::trace("recnow: 用法 --recnow x y w h [seconds] [fps]");
        return;
    }
    let secs = argv
        .get(i + 5)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(5.0);
    let fps = argv
        .get(i + 6)
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(60);

    unsafe {
        session::register();
        let class = to_wide("SmartPasteRecTest");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(rec_test_proc),
            hInstance: hinstance(),
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            to_wide("rec").as_ptr(),
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

        let rect = RECT {
            left: nums[0] as i32,
            top: nums[1] as i32,
            right: nums[0] as i32 + nums[2] as i32,
            bottom: nums[1] as i32 + nums[3] as i32,
        };
        let cfg = config::Config {
            record_fps: fps,
            record_countdown: 0,
            ..Default::default()
        };
        win::trace(&format!(
            "recnow: {}x{} at ({},{}) {}s {}fps",
            nums[2], nums[3], nums[0], nums[1], secs, fps
        ));
        if let Err(e) = session::start(hwnd, rect, &cfg) {
            win::trace(&format!("recnow FAILED: {e}"));
            return;
        }
        SetTimer(hwnd, 1, (secs * 1000.0) as u32, None);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

extern "system" fn rec_test_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                if let Some(out) = session::stop() {
                    match out {
                        session::Outcome::Ok { .. } => {}
                        session::Outcome::TooShort => win::trace("recnow: 太短，已丢弃"),
                        session::Outcome::Err(e) => win::trace(&format!("recnow FAILED: {e}")),
                    }
                }
                KillTimer(hwnd, 1);
                DestroyWindow(hwnd);
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
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
