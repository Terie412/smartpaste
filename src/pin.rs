//! 钉图窗口：置顶、可拖动、滚轮缩放、Ctrl+滚轮调透明度。
//!
//! 阴影做法：
//!   窗口比图片大一圈（PAD），外圈是软件算出来的柔和阴影 —— 像素到图片
//!   矩形的距离映射成 alpha，平方衰减。图片本身不透明，直接覆盖上去。
//!
//! 因为要逐像素控制 alpha，这里不能用 SetLayeredWindowAttributes +
//! StretchBlt（那是整窗一个透明度、且画不了半透明边缘），
//! 全程走 UpdateLayeredWindow：整张图预乘好 alpha 后一次提交。
//!
//! 透明度不用 BLENDFUNCTION 的 SourceConstantAlpha，而是直接乘进每个像素的
//! alpha 里 —— 那样滚轮提示条（HUD）才能保持不透明，否则把图调到 20% 时
//! 提示条自己也只有 20%，正好看不见。
//!
//! 滚轮提示条（HUD）：
//!   缩放 / 透明度变化时在图片底部居中浮一个深色药丸，显示当前百分比，
//!   松手后约 0.9 秒自动消失。药丸的位置、圆角、alpha 全部在像素缓冲里
//!   自己合成（预乘 alpha 的 over 运算），文字再走 GDI+ 叠上去。

use crate::win::{hinstance, to_wide};
use image::imageops::FilterType;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
    HBITMAP, HDC, HGDIOBJ,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyState, ReleaseCapture, SetCapture, VK_CONTROL, VK_SHIFT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    GetCursorPos, GetWindowRect, KillTimer, PostMessageW, RegisterClassW, SetForegroundWindow,
    SetTimer, SetWindowPos, ShowWindow, TrackPopupMenu, UpdateLayeredWindow, MF_STRING,
    SWP_NOZORDER, SW_SHOW, TPM_RETURNCMD, TPM_RIGHTBUTTON, ULW_ALPHA, WM_LBUTTONDBLCLK,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NULL, WM_RBUTTONUP, WM_TIMER,
    WNDCLASSW, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

const CLASS_NAME: &str = "SmartPastePin";
/// 图片四周留给阴影的边距
const PAD: i32 = 18;
/// 阴影最深处的 alpha（贴着图片边缘那一圈，会被图片盖住）
const SHADOW_ALPHA: f32 = 110.0;

/// 滚轮提示条停留时长（每次滚轮重置）
const HUD_HOLD_MS: u32 = 900;
/// 提示条自动消失用的定时器 ID
const TIMER_HUD: usize = 1;
/// 提示条底色 / 文字色
const HUD_BG: (u32, u32, u32) = (0x2E, 0x2E, 0x36);
const HUD_FG: u32 = 0x00FF_FFFF;

/// 缩放 / 透明度上下限（百分点）
const SCALE_MIN_PCT: f32 = 10.0;
const SCALE_MAX_PCT: f32 = 600.0;
const ALPHA_MIN_PCT: u8 = 10;
const ALPHA_MAX_PCT: u8 = 100;

/// 提示条显示的是哪一路数值
#[derive(Clone, Copy)]
enum HudKind {
    Scale,
    Alpha,
}

/// 正在显示的提示条：种类 + 到期时刻
#[derive(Clone, Copy)]
struct Hud {
    kind: HudKind,
    until: Instant,
}

/// 右键菜单项 ID（只在本模块内用，不用跟主窗口命令号对齐）
const IDM_SAVE: usize = 1;
const IDM_COPY: usize = 2;

pub struct Pin {
    pub hwnd: HWND,
    img: image::RgbaImage,
    scale: f32,
    /// 透明度，按百分点存（10~100）。存百分点而不是 0~255，是为了让
    /// 提示条上的数字和实际值严格一致，不会出现"显示 60% 实际 58%"。
    alpha_pct: u8,
    hud: Option<Hud>,
    dc: HDC,
    bmp: HBITMAP,
    dragging: bool,
    drag_cursor: POINT,
    drag_win: POINT,
}

impl Pin {
    /// 百分点 → UpdateLayeredWindow 用的 alpha
    fn alpha_u8(&self) -> u32 {
        (self.alpha_pct as u32 * 255 + 50) / 100
    }
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
            // 透明度直接乘进像素 alpha，整窗提交时恒为不透明
            let a = self.alpha_u8();
            let af = a as f32 / 255.0;

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
                    let sa = (t * t * SHADOW_ALPHA * af) as u32;
                    px[row + x as usize] = sa << 24; // 纯黑，alpha 预乘后 RGB 仍是 0
                }
            }

            // 2. 图片外沿 1px 亮边。
            //    深色壁纸上纯黑阴影几乎不可见，得靠这条白线把边界划出来；
            //    浅色背景上它又刚好补足阴影的过渡，两头都不突兀。
            let ring_a = 132 * a / 255;
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
            let scaled =
                image::imageops::resize(&self.img, dw as u32, dh as u32, FilterType::Lanczos3);
            for (y, line) in scaled.rows().enumerate() {
                let base = (y as i32 + PAD) * w + PAD;
                for (x, p) in line.enumerate() {
                    // image::RgbaImage 是 R,G,B,A；32bpp DIB 的 u32 是 0xAARRGGBB。
                    // 写成 p[2] 会让红蓝对调（黄色 (255,255,0) 变青色）。
                    // 透明度是预乘的：alpha 打折时 RGB 也得跟着除，否则
                    // UpdateLayeredWindow 会把它当直通色再乘一遍，图会发亮。
                    px[(base + x as i32) as usize] = (a << 24)
                        | ((p[0] as u32 * a / 255) << 16)
                        | ((p[1] as u32 * a / 255) << 8)
                        | (p[2] as u32 * a / 255);
                }
            }

            // 4. 滚轮提示条，叠在图片左上角
            self.draw_hud(dc, px.as_mut_ptr(), w, h);

            // 5. 提交
            let dst = POINT { x: dst_x, y: dst_y };
            let size = SIZE { cx: w, cy: h };
            let src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                // 透明度已经乘进像素，这里恒为不透明
                SourceConstantAlpha: 255,
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

    /// 在像素缓冲里叠一个深色药丸 + 百分比文字。
    ///
    /// 药丸自己算圆角覆盖率（1px 抗锯齿），按预乘 alpha 做 over 合成；
    /// 文字走 GDI+。药丸是**完全不透明**的，这样文字用 GDI+ 直接叠上去
    /// 时混合结果是正确的 —— 半透明底会让 GDI+ 把预乘过的 RGB 当直通色
    /// 再混一遍，字边会出现一圈发亮的毛边。
    ///
    /// 参数里的 w/h 是整个窗口（含阴影边距）。
    /// 缓冲用裸指针而不是切片：中间要调 GDI+（它只认 HDC），
    /// 让一个 `&mut [u32]` 跨越那个外部调用等于同时持有两把可变引用。
    fn draw_hud(&self, dc: HDC, bits: *mut u32, w: i32, h: i32) {
        let Some(hud) = self.hud else {
            return;
        };
        let text = match hud.kind {
            HudKind::Scale => format!("缩放 {}%", (self.scale * 100.0).round() as i32),
            HudKind::Alpha => format!("透明度 {}%", self.alpha_pct),
        };
        let spec = crate::gdiplus::FontSpec::new(13.0, true, "Microsoft YaHei");
        let (tw, th) = unsafe { crate::gdiplus::measure_text(dc, &spec, &text) };
        if tw <= 0 || th <= 0 {
            return;
        }
        let (pad_x, pad_y) = (10, 5);
        let (bw, bh) = (tw + pad_x * 2, th + pad_y * 2);
        if bw + 4 >= w || bh + 4 >= h {
            return; // 图太小，塞不下就不画
        }

        // 固定贴图片左上角。缩放的锚点就是左上角（窗口左上角不动，尺寸
        // 往右下长），提示条放在这个角上才能保持屏幕位置不变；放底部居中
        // 的话每缩放一格它自己也跟着往下跑，眼睛得追着看。
        // 透明度和缩放用同一个位置，免得同一个手势出来的提示在两个角之间跳。
        let bx = (PAD + 10).min(w - bw - 2);
        let by = (PAD + 10).min(h - bh - 2);

        let radius = bh as f32 / 2.0;
        for y in 0..bh {
            let wy = by + y;
            if wy < 0 || wy >= h {
                continue;
            }
            for x in 0..bw {
                let wx = bx + x;
                if wx < 0 || wx >= w {
                    continue;
                }
                let cov =
                    round_coverage(x as f32 + 0.5, y as f32 + 0.5, bw as f32, bh as f32, radius);
                if cov <= 0.0 {
                    continue;
                }
                over(
                    unsafe { &mut *bits.add((wy * w + wx) as usize) },
                    HUD_BG,
                    1.0,
                    cov,
                );
            }
        }

        unsafe {
            crate::gdiplus::draw_text(dc, &spec, HUD_FG, bx + pad_x, by + pad_y, &text);
        }
    }

    /// 右键菜单：另存为 / 复制。
    ///
    /// 用系统原生 TrackPopupMenu，不自己画——菜单字体、圆角、动画、
    /// 键盘导航（上下键 + 回车）全部自动跟系统走。
    fn show_menu(&mut self) {
        unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            let save_label = to_wide("另存为(&S)");
            let copy_label = to_wide("复制(&C)");
            AppendMenuW(menu, MF_STRING, IDM_SAVE, save_label.as_ptr());
            AppendMenuW(menu, MF_STRING, IDM_COPY, copy_label.as_ptr());

            let mut pt: POINT = std::mem::zeroed();
            GetCursorPos(&mut pt);
            // TrackPopupMenu 要求 owner 是前台窗口，否则菜单弹出后
            // 点窗口外面它不会消失，会一直挂在屏幕上。
            SetForegroundWindow(self.hwnd);
            let cmd = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                pt.x,
                pt.y,
                0,
                self.hwnd,
                std::ptr::null(),
            );
            // 配套的经典 workaround：让菜单彻底退出前的前台状态归还给系统
            PostMessageW(self.hwnd, WM_NULL, 0, 0);
            DestroyMenu(menu);

            match cmd as usize {
                IDM_SAVE => self.save_as(),
                IDM_COPY => self.copy(),
                _ => {}
            }
        }
    }

    /// 另存为：走和遮罩里同一个对话框，成功后记住目录。
    /// 存的是原始分辨率的图，不是滚轮缩放后的显示尺寸。
    fn save_as(&self) {
        let cfg = crate::config::Config::load();
        let name = crate::savedlg::default_name();
        // 菜单刚关闭时前台归属还在交接中，直接弹模态框可能出现
        // "对话框在但键盘没反应"，先把前台抢回来。owner 必须是 pin 窗口：
        // pin 是 topmost，owner 传 NULL 的话对话框会被它盖住。
        unsafe { SetForegroundWindow(self.hwnd) };
        let Some(path) = crate::savedlg::ask(self.hwnd, cfg.save_dir.as_deref(), &name) else {
            crate::win::trace("pin save: 用户取消");
            return;
        };
        if self.img.save(&path).is_err() {
            crate::win::trace(&format!("pin save: 写入失败 {}", path.display()));
            return;
        }
        let mut cfg = cfg;
        cfg.save_dir = path
            .parent()
            .map(|d| d.display().to_string())
            .filter(|d| !d.is_empty());
        cfg.save();
        crate::win::trace(&format!("pin saved {}", path.display()));
    }

    fn copy(&self) {
        match crate::clipboard::copy_image(&self.img) {
            Ok(()) => crate::win::trace("pin copy ok"),
            Err(e) => crate::win::trace(&format!("pin copy failed: {e}")),
        }
    }
}

static mut PINS: Vec<Box<Pin>> = Vec::new();

/// 圆角矩形的像素覆盖率（1px 抗锯齿）。
///
/// 用有符号距离场：内部为负、边缘为 0、外部为正，`0.5 - d` 直接就是覆盖率。
/// 半径取 bh/2 时退化成胶囊形。
fn round_coverage(x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    let (cx, cy) = (w / 2.0, h / 2.0);
    let qx = (x - cx).abs() - (cx - r);
    let qy = (y - cy).abs() - (cy - r);
    let d = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r;
    (0.5 - d).clamp(0.0, 1.0)
}

/// 预乘 alpha 的 over 合成：dst = src + dst * (1 - srcA)。
///
/// 目标缓冲是 UpdateLayeredWindow 要求的样子，RGB 已经按 alpha 预乘过，
/// 所以这里不能先除回去再乘，直接按预乘公式算。
fn over(dst: &mut u32, color: (u32, u32, u32), alpha: f32, coverage: f32) {
    let sa = (alpha * coverage).clamp(0.0, 1.0);
    if sa <= 0.0 {
        return;
    }
    let inv = 1.0 - sa;
    let (r, g, b) = (color.0 as f32, color.1 as f32, color.2 as f32);
    let dr = ((*dst >> 16) & 0xFF) as f32;
    let dg = ((*dst >> 8) & 0xFF) as f32;
    let db = (*dst & 0xFF) as f32;
    let da = ((*dst >> 24) & 0xFF) as f32;
    let nr = (r * sa + dr * inv).min(255.0) as u32;
    let ng = (g * sa + dg * inv).min(255.0) as u32;
    let nb = (b * sa + db * inv).min(255.0) as u32;
    let na = (sa * 255.0 + da * inv).min(255.0) as u32;
    *dst = (na << 24) | (nr << 16) | (ng << 8) | nb;
}

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
                        SetWindowPos(
                            hwnd,
                            std::ptr::null_mut(),
                            nx,
                            ny,
                            0,
                            0,
                            SWP_NOZORDER | 0x0001,
                        );
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
                let up = delta > 0;
                let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
                // Shift 用 GetAsyncKeyState：修饰键的状态跟本线程的消息队列无关，
                // GetKeyState 在读不到的时候会返回 0（录热键时踩过一次）。
                let fine = GetAsyncKeyState(VK_SHIFT as i32) < 0;

                let mut wr: RECT = std::mem::zeroed();
                GetWindowRect(hwnd, &mut wr);
                let (wx, wy) = (wr.left, wr.top);

                if let Some(p) = find_mut(hwnd) {
                    // 两种缩放都是**以 100% 为基准**做加减，不乘当前值：
                    // 常规每格 ±10 个百分点（100→110→120），Shift 精细 ±1。
                    // 乘 1.1 那种写法看着像"每格涨 10%"，其实基数在变——
                    // 到 200% 时一格能跳 20%，越放越粗。
                    let step = if fine { 1 } else { 10 };
                    let dir = if up { 1 } else { -1 };
                    let kind = if ctrl {
                        let v = p.alpha_pct as i32 + step * dir;
                        p.alpha_pct = v.clamp(ALPHA_MIN_PCT as i32, ALPHA_MAX_PCT as i32) as u8;
                        HudKind::Alpha
                    } else {
                        let pct = (p.scale * 100.0).round() + (step * dir) as f32;
                        p.scale = (pct / 100.0).clamp(SCALE_MIN_PCT / 100.0, SCALE_MAX_PCT / 100.0);
                        HudKind::Scale
                    };
                    p.hud = Some(Hud {
                        kind,
                        until: Instant::now() + Duration::from_millis(HUD_HOLD_MS as u64),
                    });
                    // 缩放和透明度都得重画整张合成位图
                    p.render(wx, wy);
                    // 每次滚轮都重置停留时间，松手后才开始倒计时
                    SetTimer(hwnd, TIMER_HUD, HUD_HOLD_MS, None);
                }
                0
            }
            // 提示条到点自己撤掉
            WM_TIMER => {
                if wparam == TIMER_HUD {
                    let expired = match find_mut(hwnd) {
                        Some(p) => match p.hud {
                            Some(hud) => Instant::now() >= hud.until,
                            None => false,
                        },
                        None => false,
                    };
                    if expired {
                        if let Some(p) = find_mut(hwnd) {
                            p.hud = None;
                            let mut wr: RECT = std::mem::zeroed();
                            GetWindowRect(hwnd, &mut wr);
                            p.render(wr.left, wr.top);
                        }
                    }
                    KillTimer(hwnd, TIMER_HUD);
                }
                0
            }

            // 右键弹菜单（另存为 / 复制），不删图
            WM_RBUTTONUP => {
                if let Some(p) = find_mut(hwnd) {
                    p.show_menu();
                }
                0
            }
            // 双击才是删除
            WM_LBUTTONDBLCLK => {
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
    PINS.iter_mut().find(|p| p.hwnd == hwnd).map(|b| b.as_mut())
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
            alpha_pct: ALPHA_MAX_PCT,
            hud: None,
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
