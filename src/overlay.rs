//! 全屏遮罩窗口：框选 + 标注 + 工具条。
//!
//! 绘制策略：
//!   1. 把抓好的 DIB 一次性 BitBlt 到 back buffer
//!   2. 用 1x1 黑位图 AlphaBlend 出半透明蒙层（只蒙选区外，省时间）
//!   3. 画标注元素（裁剪到选区内）
//!   4. 画边框、尺寸标签、工具条、属性条
//!   5. 整个 back buffer 一次 BitBlt 到窗口 DC
//!
//! 全程 GDI，没有纹理上传，没有 GPU。

use crate::capture::{self, Screen};
use crate::edit::{self, Doc, Kind, Shape, Style};
use crate::icons::IconSet;
use crate::win::{delete_font, hinstance, pt_in, rect, to_wide, ui_font, ui_font_w};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows_sys::Win32::Graphics::Gdi::{
    AlphaBlend, BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreatePen,
    CreateSolidBrush, DeleteDC, DeleteObject, EndPaint, FillRect, GetDC, GetStockObject,
    GetTextExtentPoint32W, GetTextMetricsW, IntersectClipRect, InvalidateRect, Rectangle,
    ReleaseDC, RestoreDC, SaveDC, ScreenToClient, SelectObject, SetBkMode, SetTextColor, TextOutW,
    UpdateWindow, AC_SRC_OVER, BLENDFUNCTION, HDC, HFONT, HGDIOBJ, HOLLOW_BRUSH, NULL_BRUSH,
    PAINTSTRUCT, PS_SOLID, SRCCOPY, TEXTMETRICW, TRANSPARENT,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_C, VK_CONTROL,
    VK_ESCAPE, VK_RETURN, VK_S, VK_SHIFT, VK_Z,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    ChildWindowFromPointEx, CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect,
    GetCursorPos, GetForegroundWindow, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    LoadCursorW, PostMessageW, RegisterClassW, SendMessageW, SetCursor, SetForegroundWindow,
    SetWindowPos, ShowWindow, CWP_SKIPINVISIBLE, CWP_SKIPTRANSPARENT, EC_LEFTMARGIN,
    EC_RIGHTMARGIN, EN_CHANGE, ES_MULTILINE, ES_NOHIDESEL, ES_WANTRETURN, HCURSOR, IDC_SIZEALL,
    IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER,
    SW_SHOW, WM_APP, WM_COMMAND, WM_CTLCOLOREDIT, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_PAINT, WM_RBUTTONUP, WM_SETCURSOR, WNDCLASSW, WS_CHILD, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

// EDIT 的子类化走 comctl32。
// 用 SetWindowLongPtr(GWLP_WNDPROC) 拿到的「原过程」在系统控件上不可靠：
// 转发 WM_GETTEXTLENGTH 会稳定返回 0，读出来的文字永远是空的。
#[link(name = "comctl32")]
extern "system" {
    fn SetWindowSubclass(hwnd: HWND, proc: SUBCLASSPROC, id: usize, data: usize) -> i32;
    fn RemoveWindowSubclass(hwnd: HWND, proc: SUBCLASSPROC, id: usize) -> i32;
    fn DefSubclassProc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT;
}

type SUBCLASSPROC =
    Option<unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM, usize, usize) -> LRESULT>;

pub const CMD_NONE: u32 = 0;
pub const CMD_COPY: u32 = 1;
pub const CMD_SAVE: u32 = 2;
pub const CMD_PIN: u32 = 3;
pub const CMD_CANCEL: u32 = 4;

/// 遮罩窗口结束后，把结果发回主窗口
pub const WM_SNAP_DONE: u32 = WM_APP + 2;
/// windows-sys 没开 Controls feature，这几个常量本地定义
const EM_SETMARGINS: u32 = 0x00D3;
const EM_GETLINECOUNT: u32 = 0x00BA;
const WM_SETFONT: u32 = 0x0030;

/// 文本输入结束（来自 EDIT 子类）
const WM_TEXT_COMMIT: u32 = WM_APP + 5;
/// 取消文本输入
const WM_TEXT_CANCEL: u32 = WM_APP + 6;

const CLASS_NAME: &str = "SmartPasteOverlay";
const MASK_ALPHA: u8 = 128;

/// UI 整体缩放。所有尺寸都过这个函数，改一处全跟着变。
///
/// 用 8/5 这种整数比而不是 f32：`const` 里的浮点运算限制多，
/// 而且整数算出来的结果稳定，不会因为平台差异抖动。
const UI_SCALE: (i32, i32) = (8, 5);

const fn ui(v: i32) -> i32 {
    v * UI_SCALE.0 / UI_SCALE.1
}

// 圆角直径（GDI+ 里再除以 2 得到半径）
const RADIUS_BAR: i32 = ui(16);
/// 选中态底色。不用描边——深色边框在白色工具条上太跳，加深底色就够了。
const SEL_BG: u32 = 0x00EB_E3E3; // #E3E3EB
const RADIUS_CHIP: i32 = ui(12);
/// 属性格底块用的小圆角。36px 的格子上用 RADIUS_CHIP 会糊成胶囊。
const RADIUS_CELL: i32 = ui(8);

// 正方形按钮：宽高一致，比原来的横向长条紧凑
const BTN_W: i32 = ui(34);
const BTN_H: i32 = ui(34);
const GAP: i32 = ui(8);

/// 属性条上一格的大小。
/// 取 32 让 12 格 = 384，和工具栏 7*54=378 基本对齐
/// 属性格是正方形（36×36）。12 格正好和 8 个工具按钮等宽（432），两条边能对齐。
const PROP_W: i32 = 36;
const PROP_H: i32 = 36;
/// 分组竖线占的窄格
const SEP_W: i32 = ui(5);

/// 预设色（COLORREF：0x00BBGGRR）
const COLORS: [u32; 6] = [
    0x0063_2EFF, // #FF2E63
    0x0000_C8FF, // #FFC800
    0x00FF_8F00, // #008FFF
    0x0078_C850, // #50C878
    0x00FF_FFFF, // 白
    0x0000_0000, // 黑
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    Shape,
    Arrow,
    Pen,
    Text,
    Eraser,
}

impl Tool {
    fn idx(self) -> usize {
        match self {
            Tool::Shape => 0,
            Tool::Arrow => 1,
            Tool::Pen => 2,
            Tool::Text => 3,
            Tool::Eraser => 4,
        }
    }

    /// 每个工具只列自己用得上的设置，互不干扰。
    /// 属性条就是按这个列表生成的，所以不存在「箭头上出现填充开关」这种事。
    fn props(self) -> &'static [Prop] {
        const COLORS_6: [Prop; 6] = [
            Prop::Color(0),
            Prop::Color(1),
            Prop::Color(2),
            Prop::Color(3),
            Prop::Color(4),
            Prop::Color(5),
        ];
        const WIDTHS_3: [Prop; 3] = [Prop::Width(0), Prop::Width(1), Prop::Width(2)];

        static SHAPE: [Prop; 15] = [
            // 形状 | 颜色 | 填充 | 粗细
            Prop::Round(false),
            Prop::Round(true),
            Prop::Sep,
            Prop::Color(0),
            Prop::Color(1),
            Prop::Color(2),
            Prop::Color(3),
            Prop::Color(4),
            Prop::Color(5),
            Prop::Sep,
            Prop::Filled,
            Prop::Sep,
            Prop::Width(0),
            Prop::Width(1),
            Prop::Width(2),
        ];
        static ARROW: [Prop; 12] = [
            // 颜色 | 线型 | 粗细
            Prop::Color(0),
            Prop::Color(1),
            Prop::Color(2),
            Prop::Color(3),
            Prop::Color(4),
            Prop::Color(5),
            Prop::Sep,
            Prop::Dashed,
            Prop::Sep,
            Prop::Width(0),
            Prop::Width(1),
            Prop::Width(2),
        ];
        static PEN: [Prop; 10] = [
            // 颜色 | 粗细
            Prop::Color(0),
            Prop::Color(1),
            Prop::Color(2),
            Prop::Color(3),
            Prop::Color(4),
            Prop::Color(5),
            Prop::Sep,
            Prop::Width(0),
            Prop::Width(1),
            Prop::Width(2),
        ];
        static TEXT: [Prop; 14] = [
            // 颜色 | 字号 | 字效
            Prop::Color(0),
            Prop::Color(1),
            Prop::Color(2),
            Prop::Color(3),
            Prop::Color(4),
            Prop::Color(5),
            Prop::Sep,
            Prop::Width(0),
            Prop::Width(1),
            Prop::Width(2),
            Prop::Sep,
            Prop::Bold,
            Prop::Stroke,
            Prop::Shadow,
        ];
        static ERASER: [Prop; 6] = [
            // 粗细 | 形状
            Prop::Width(0),
            Prop::Width(1),
            Prop::Width(2),
            Prop::Sep,
            Prop::Round(false),
            Prop::Round(true),
        ];

        let _ = (COLORS_6, WIDTHS_3);
        match self {
            Tool::Shape => &SHAPE,
            Tool::Arrow => &ARROW,
            Tool::Pen => &PEN,
            Tool::Text => &TEXT,
            Tool::Eraser => &ERASER,
        }
    }

    /// 笔触档位因工具而异：橡皮明显要粗得多，文本这一档是字号
    fn width_at(self, i: usize) -> i32 {
        match self {
            Tool::Eraser => [12, 24, 40][i.min(2)],
            Tool::Text => [18, 26, 34][i.min(2)],
            _ => [2, 4, 7][i.min(2)],
        }
    }
}

/// 属性条上的一格
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Prop {
    Color(usize),
    Width(usize),
    Filled,
    Dashed,
    /// true = 圆/椭圆，false = 矩形
    Round(bool),
    /// 文本加粗
    Bold,
    /// 文本描边
    Stroke,
    /// 文本投影
    Shadow,
    /// 纯视觉分隔，不响应点击
    Sep,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Tool(Tool),
    Copy,
    Save,
    Pin,
}

pub struct Button {
    pub rect: RECT,
    pub act: Act,
    /// 对应 icons.png 里的第几格
    pub icon: usize,
}

/// 锁定后的拖拽类型
#[derive(Clone, Copy, PartialEq, Eq)]
enum Grab {
    None,
    /// 拖选区内部：整体移动
    Move,
    /// 拖边/角：位掩码 1 左 2 右 4 上 8 下
    Edge(u8),
}

pub struct Overlay {
    pub hwnd: HWND,
    pub screen: Screen,
    pub dragging: bool,
    pub locked: bool,
    pub start: POINT,
    pub cur: POINT,
    pub hover: i32,
    pub hover_prop: i32,
    pub pressed: i32,
    pub buttons: Vec<Button>,
    pub props: Vec<(RECT, Prop)>,
    pub tool: Option<Tool>,
    /// 每个工具各自一套设置，切换工具不会串味
    pub styles: [Style; 5],
    /// 光标的屏幕坐标，取色用
    pub cursor: POINT,
    /// 取色显示模式：false = RGB，true = Hex
    pub hex_mode: bool,
    pub draft: Option<Shape>,
    pub drawing: bool,
    /// 预测出来的区域（未锁定时的悬停高亮）
    pub predict: Option<RECT>,
    /// 打开遮罩之前的前台窗口，预测就从它算
    prev_fg: HWND,
    /// 锁定状态下正在拖选区（移动或改边）
    grab: Grab,
    grab_rect: RECT,
    grab_from: POINT,
    cur_we: HCURSOR,
    cur_ns: HCURSOR,
    cur_nwse: HCURSOR,
    cur_nesw: HCURSOR,
    cur_all: HCURSOR,
    pub doc: Doc,
    /// 正在输入的 EDIT 子窗口，null 表示没在编辑
    text_edit: HWND,
    /// 文字锚点，相对选区左上角
    text_at: POINT,
    edit_font: HFONT,
    icons: IconSet,
    back_dc: HDC,
    back_bmp: windows_sys::Win32::Graphics::Gdi::HBITMAP,
    mask_dc: HDC,
    mask_bmp: windows_sys::Win32::Graphics::Gdi::HBITMAP,
    label_font: HFONT,
}

static mut OVERLAY: Option<Box<Overlay>> = None;
/// 等待主窗口处理的命令
static mut PENDING_CMD: u32 = CMD_NONE;
/// 裁剪结果（交给主窗口做复制/保存/钉图）
static mut PENDING_IMAGE: Option<image::RgbaImage> = None;
static mut PENDING_ORIGIN: POINT = POINT { x: 0, y: 0 };
static mut MAIN_HWND: HWND = std::ptr::null_mut();
/// 调试用：启动后直接锁定的选区 (x, y, w, h)
static mut PRESET: Option<(i32, i32, i32, i32)> = None;

/// 让下一次弹出的遮罩直接锁定一块区域，跳过鼠标拖拽。
///
/// 模拟鼠标在 DPI 缩放下坐标不可靠，调样式时用它截图更稳。
pub fn set_preset(x: i32, y: i32, w: i32, h: i32) {
    unsafe {
        PRESET = Some((x, y, w, h));
    }
}

pub fn set_main_hwnd(h: HWND) {
    unsafe {
        MAIN_HWND = h;
    }
}

/// 取屏幕坐标下的像素。坐标落在外就返回 None。
///
/// DIB 里的 u32 是 0x00RRGGBB（小端存成 B,G,R,0），和高位取色的顺序一致。
fn sample_pixel(screen: &Screen, x: i32, y: i32) -> Option<(u8, u8, u8)> {
    let lx = x - screen.x;
    let ly = y - screen.y;
    if lx < 0 || ly < 0 || lx >= screen.width || ly >= screen.height {
        return None;
    }
    let idx = (ly * screen.width + lx) as usize;
    let px = unsafe { *screen.bits.add(idx) };
    Some((
        ((px >> 16) & 0xFF) as u8,
        ((px >> 8) & 0xFF) as u8,
        (px & 0xFF) as u8,
    ))
}

fn color_text(r: u8, g: u8, b: u8, hex: bool) -> String {
    if hex {
        format!("#{r:02X}{g:02X}{b:02X}")
    } else {
        format!("{r}, {g}, {b}")
    }
}

/// EDIT 的子类过程：只截 Enter / Esc，其余原样转发。
///
/// 不能在这里直接销毁 EDIT——那是它自己的窗口过程，销毁后返回会踩到野指针，
/// 所以只往遮罩窗口投一条消息，让外层去收尾。
unsafe extern "system" fn text_edit_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if msg == WM_KEYDOWN {
        let vk = wparam as u32;
        // 用 GetAsyncKeyState 读物理状态：GetKeyState 依赖本线程队列的处理进度，
        // 注入的按键经常读不到（设置面板录热键时已经踩过一次）
        if vk == VK_RETURN as u32 && GetAsyncKeyState(VK_SHIFT as i32) >= 0 {
            if let Some(ov) = OVERLAY.as_ref() {
                PostMessageW(ov.hwnd, WM_TEXT_COMMIT, 0, 0);
            }
            return 0;
        }
        if vk == VK_ESCAPE as u32 {
            if let Some(ov) = OVERLAY.as_ref() {
                PostMessageW(ov.hwnd, WM_TEXT_CANCEL, 0, 0);
            }
            return 0;
        }
    }
    // 输入框要"透明"：擦背景时把 back buffer 里对应的那块拷进来。
    // 这样 EDIT 仍是正常可见窗口（输入法才能算对候选窗位置），
    // 屏幕上看到的却是截图本身，不是白块。
    if msg == 0x0014 {
        if let Some(ov) = OVERLAY.as_ref() {
            let hdc = wparam as HDC;
            let mut cr: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut cr);
            let mut wr: RECT = std::mem::zeroed();
            GetWindowRect(hwnd, &mut wr);
            BitBlt(
                hdc,
                0,
                0,
                cr.right,
                cr.bottom,
                ov.back_dc,
                wr.left - ov.screen.x,
                wr.top - ov.screen.y,
                SRCCOPY,
            );
        }
        return 1;
    }

    DefSubclassProc(hwnd, msg, wparam, lparam)
}

/// 窗口的真实可见边界。
///
/// Win10 起 GetWindowRect 会把四周那圈看不见的拖拽边框也算进去（约 7px），
/// 直接用会比看到的窗口大一圈。DWM 的 EXTENDED_FRAME_BOUNDS 才是视觉边界，
/// 拿不到再退回 GetWindowRect。
unsafe fn window_bounds(hwnd: HWND) -> Option<RECT> {
    if hwnd.is_null() {
        return None;
    }
    let mut r: RECT = std::mem::zeroed();
    let rc = DwmGetWindowAttribute(
        hwnd,
        DWMWA_EXTENDED_FRAME_BOUNDS as u32,
        &mut r as *mut RECT as *mut core::ffi::c_void,
        std::mem::size_of::<RECT>() as u32,
    );
    if rc >= 0 && r.right > r.left && r.bottom > r.top {
        return Some(r);
    }
    let mut g: RECT = std::mem::zeroed();
    if GetWindowRect(hwnd, &mut g) != 0 && g.right > g.left {
        return Some(g);
    }
    None
}

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

fn lo(v: LPARAM) -> i32 {
    (v as usize & 0xFFFF) as u16 as i16 as i32
}
fn hi(v: LPARAM) -> i32 {
    ((v as usize >> 16) & 0xFFFF) as u16 as i16 as i32
}

impl Overlay {
    fn selection(&self) -> Option<RECT> {
        if !self.locked && !self.dragging {
            return None;
        }
        let l = self.start.x.min(self.cur.x);
        let t = self.start.y.min(self.cur.y);
        let r = self.start.x.max(self.cur.x);
        let b = self.start.y.max(self.cur.y);
        if r - l < 1 || b - t < 1 {
            return None;
        }
        Some(rect(l, t, r, b))
    }

    /// 工具按钮 + 操作按钮，整体右对齐选区
    fn layout_buttons(&mut self) {
        self.buttons.clear();
        let Some(sel) = self.selection() else {
            return;
        };

        let items = [
            (Act::Tool(Tool::Shape), 4usize),
            (Act::Tool(Tool::Arrow), 3),
            (Act::Tool(Tool::Pen), 6),
            (Act::Tool(Tool::Text), 7),
            (Act::Tool(Tool::Eraser), 8),
            (Act::Save, 1),
            (Act::Pin, 2),
            (Act::Copy, 0),
        ];

        let total = items.len() as i32 * BTN_W;
        let mut x = sel.right - total;
        if x < 0 {
            x = 0;
        }
        let mut y = sel.bottom + GAP;
        if y + BTN_H + ui(4) > self.screen.height {
            y = sel.top - BTN_H - GAP;
        }
        if y < 4 {
            y = (sel.bottom + GAP).min(self.screen.height - BTN_H - ui(4));
        }

        for (act, icon) in items {
            self.buttons.push(Button {
                rect: rect(x, y, x + BTN_W, y + BTN_H),
                act,
                icon,
            });
            x += BTN_W;
        }

        // 属性条：内容完全由当前工具决定
        self.props.clear();
        let Some(tool) = self.tool else {
            return;
        };
        let list = tool.props();
        let pw: i32 = list
            .iter()
            .map(|p| if *p == Prop::Sep { SEP_W } else { PROP_W })
            .sum();
        let mut px = sel.right - pw;
        if px < 0 {
            px = 0;
        }
        // 跟工具栏同侧：工具栏在下就排它下面，在上就排它上面，
        // 避免跑进选区里盖住内容
        let bar_y = self.buttons[0].rect.top;
        let below = bar_y > sel.bottom;
        let mut py = if below {
            bar_y + BTN_H + GAP
        } else {
            bar_y - PROP_H - GAP
        };
        if py < 4 {
            py = bar_y + BTN_H + GAP;
        }
        if py + PROP_H + 4 > self.screen.height {
            py = bar_y - PROP_H - GAP;
        }
        for prop in list {
            let w = if *prop == Prop::Sep { SEP_W } else { PROP_W };
            self.props.push((rect(px, py, px + w, py + PROP_H), *prop));
            px += w;
        }
        crate::win::trace(&format!(
            "layout: {} buttons, {} props, tool={:?}, bar=({},{}), props_y={}",
            self.buttons.len(),
            self.props.len(),
            tool,
            self.buttons[0].rect.left,
            self.buttons[0].rect.top,
            py - PROP_H - GAP
        ));
    }

    /// 预测光标下的区域。
    ///
    /// 只做到"窗口"这一级：打开遮罩前记住的前台窗口，再用
    /// ChildWindowFromPointEx 细化到子窗口（去掉标题栏/菜单）。
    /// 不追求元素级精度——预测本来就是提示性质的，而窗口级零依赖、
    /// 零延迟、不会闪，也不会干扰任何现有交互。
    unsafe fn refresh_predict(&mut self, x: i32, y: i32) {
        self.predict = None;
        if self.prev_fg.is_null() {
            return;
        }

        let (sx, sy) = (x + self.screen.x, y + self.screen.y);
        let Some(wr) = window_bounds(self.prev_fg) else {
            return;
        };
        if sx < wr.left || sx >= wr.right || sy < wr.top || sy >= wr.bottom {
            return;
        }

        // 细化到光标所在的那个子窗口：浏览器/编辑器的客户区都在子窗口里
        let mut pt = POINT { x: sx, y: sy };
        ScreenToClient(self.prev_fg, &mut pt);
        let child =
            ChildWindowFromPointEx(self.prev_fg, pt, CWP_SKIPINVISIBLE | CWP_SKIPTRANSPARENT);
        let target = if child.is_null() { self.prev_fg } else { child };
        let mut r = window_bounds(target).unwrap_or(wr);

        // 夹到屏幕里
        r.left = r.left.max(self.screen.x);
        r.top = r.top.max(self.screen.y);
        r.right = r.right.min(self.screen.x + self.screen.width);
        r.bottom = r.bottom.min(self.screen.y + self.screen.height);
        if r.right - r.left < 8 || r.bottom - r.top < 8 {
            return;
        }
        self.predict = Some(r);
    }

    /// 预测区域转换成客户区坐标（back buffer 坐标）
    fn predict_client(&self) -> Option<RECT> {
        let r = self.predict?;
        Some(rect(
            r.left - self.screen.x,
            r.top - self.screen.y,
            r.right - self.screen.x,
            r.bottom - self.screen.y,
        ))
    }

    /// 点落在选区的哪条边上（位掩码），不在边带内返回 0
    fn edge_zone(&self, s: &RECT, x: i32, y: i32) -> u8 {
        let band = ui(4);
        if x < s.left - band || x > s.right + band || y < s.top - band || y > s.bottom + band {
            return 0;
        }
        let mut z = 0;
        if (x - s.left).abs() <= band {
            z |= 1;
        }
        if (x - s.right).abs() <= band {
            z |= 2;
        }
        if (y - s.top).abs() <= band {
            z |= 4;
        }
        if (y - s.bottom).abs() <= band {
            z |= 8;
        }
        z
    }

    fn grab_cursor(&self, zone: u8) -> Option<HCURSOR> {
        let (l, r, t, b) = (zone & 1 != 0, zone & 2 != 0, zone & 4 != 0, zone & 8 != 0);
        if (l && t) || (r && b) {
            return Some(self.cur_nwse);
        }
        if (r && t) || (l && b) {
            return Some(self.cur_nesw);
        }
        if l || r {
            return Some(self.cur_we);
        }
        if t || b {
            return Some(self.cur_ns);
        }
        None
    }

    unsafe fn begin_grab(&mut self, x: i32, y: i32, zone: u8) {
        let Some(s) = self.selection() else { return };
        self.grab = if zone == 0 {
            Grab::Move
        } else {
            Grab::Edge(zone)
        };
        self.grab_rect = s;
        self.grab_from = POINT { x, y };
        self.hover = -1;
        self.hover_prop = -1;
        SetCursor(self.grab_cursor(zone).unwrap_or(self.cur_all));
        crate::win::trace(&format!(
            "sel: grab zone={zone} rect=({},{},{},{})",
            s.left, s.top, s.right, s.bottom
        ));
    }

    unsafe fn grab_update(&mut self, x: i32, y: i32) {
        if self.grab == Grab::None {
            return;
        }
        let (dx, dy) = (x - self.grab_from.x, y - self.grab_from.y);
        if dx == 0 && dy == 0 {
            return;
        }
        self.grab_from = POINT { x, y };

        let old = self.grab_rect;
        let mut r = old;
        let min = ui(8);
        match self.grab {
            Grab::Move => {
                let w = r.right - r.left;
                let h = r.bottom - r.top;
                r.left = (r.left + dx).max(0).min((self.screen.width - w).max(0));
                r.top = (r.top + dy).max(0).min((self.screen.height - h).max(0));
                r.right = r.left + w;
                r.bottom = r.top + h;
            }
            Grab::Edge(z) => {
                if z & 1 != 0 {
                    r.left = (r.left + dx).max(0).min((r.right - min).max(0));
                }
                if z & 2 != 0 {
                    r.right = (r.right + dx).max(r.left + min).min(self.screen.width);
                }
                if z & 4 != 0 {
                    r.top = (r.top + dy).max(0).min((r.bottom - min).max(0));
                }
                if z & 8 != 0 {
                    r.bottom = (r.bottom + dy).max(r.top + min).min(self.screen.height);
                }
            }
            Grab::None => {}
        }

        // 元素坐标相对选区左上角，左上角一动就得反向平移，标注才钉在屏幕原处
        self.doc.shift(old.left - r.left, old.top - r.top);
        self.grab_rect = r;
        self.start = POINT {
            x: r.left,
            y: r.top,
        };
        self.cur = POINT {
            x: r.right,
            y: r.bottom,
        };
        self.layout_buttons();
        InvalidateRect(self.hwnd, std::ptr::null(), 0);
    }

    unsafe fn end_grab(&mut self) {
        if self.grab == Grab::None {
            return;
        }
        self.grab = Grab::None;
        ReleaseCapture();
        self.layout_buttons();
        InvalidateRect(self.hwnd, std::ptr::null(), 0);
    }

    fn hit_button(&self, x: i32, y: i32) -> i32 {
        if !self.locked {
            return -1;
        }
        self.buttons
            .iter()
            .position(|b| pt_in(&b.rect, x, y))
            .map(|i| i as i32)
            .unwrap_or(-1)
    }

    fn hit_prop(&self, x: i32, y: i32) -> i32 {
        if !self.locked || self.tool.is_none() {
            return -1;
        }
        self.props
            .iter()
            .position(|(r, p)| !matches!(p, Prop::Sep) && pt_in(r, x, y))
            .map(|i| i as i32)
            .unwrap_or(-1)
    }

    fn finish(&mut self, cmd: u32) {
        if cmd == CMD_COPY || cmd == CMD_SAVE || cmd == CMD_PIN {
            let Some(sel) = self.selection() else {
                return;
            };
            let Some(base) = self.screen.crop_rgba(
                sel.left,
                sel.top,
                sel.right - sel.left,
                sel.bottom - sel.top,
            ) else {
                return;
            };
            // 有标注的话，从 back buffer 把合成结果取回来，否则直接用底图
            let img = unsafe {
                match read_back_buffer(self.back_dc, sel) {
                    Some(composed) if !self.doc.is_empty() => composed,
                    _ => base,
                }
            };
            unsafe {
                PENDING_IMAGE = Some(img);
                PENDING_ORIGIN = POINT {
                    x: self.screen.x + sel.left,
                    y: self.screen.y + sel.top,
                };
            }
        }
        unsafe {
            PENDING_CMD = cmd;
            crate::win::trace(&format!(
                "overlay: finish cmd={cmd} sel={:?}",
                self.selection()
                    .map(|r| (r.left, r.top, r.right - r.left, r.bottom - r.top))
            ));
            if !MAIN_HWND.is_null() {
                PostMessageW(MAIN_HWND, WM_SNAP_DONE, cmd as usize, 0);
            } else {
                DestroyWindow(self.hwnd);
            }
        }
    }

    /// 在点击处拉起一个 EDIT 子窗口输入文字。
    ///
    /// 用系统 EDIT 而不是自己收 WM_CHAR：中文输入法需要 IME 组字、候选窗、
    /// 光标跟随，这套东西只有 EDIT 控件是现成的。
    unsafe fn start_text(&mut self, x: i32, y: i32) {
        let Some(sel) = self.selection() else { return };
        if !pt_in(&sel, x, y) {
            return;
        }
        self.commit_text();

        let st = self.styles[Tool::Text.idx()];
        let px = st.width.max(10);
        self.edit_font = ui_font_w(px, if st.bold { 700 } else { 400 });

        let lh = (px as f32 * 1.4) as i32;
        // EDIT 有个 1~2px 的内部缩进，往左上让一点，让它的首字落在点击处
        let pad = 2;
        let edit = CreateWindowExW(
            0,
            to_wide("EDIT").as_ptr(),
            std::ptr::null(),
            WS_CHILD
                | WS_VISIBLE
                | (ES_MULTILINE as u32)
                | (ES_WANTRETURN as u32)
                | (ES_NOHIDESEL as u32),
            x - pad,
            y - ui(2),
            ui(220),
            lh + ui(6),
            self.hwnd,
            1 as *mut core::ffi::c_void,
            hinstance(),
            std::ptr::null_mut(),
        );
        if edit.is_null() {
            crate::win::trace("text: create EDIT failed");
            self.destroy_edit();
            return;
        }

        SendMessageW(edit, WM_SETFONT, self.edit_font as WPARAM, 1);
        let margins = 0u32;
        SendMessageW(
            edit,
            EM_SETMARGINS,
            (EC_LEFTMARGIN | EC_RIGHTMARGIN) as WPARAM,
            margins as LPARAM,
        );

        self.text_edit = edit;
        self.text_at = POINT {
            x: x - sel.left,
            y: y - sel.top,
        };
        let ok = SetWindowSubclass(edit, Some(text_edit_proc), 1, 0);
        crate::win::trace(&format!("text: edit={edit:?} subclass={ok}"));
        SetFocus(edit);
        crate::win::trace(&format!("text: begin at {x},{y}"));
    }

    /// 结束输入并把文字落成元素
    unsafe fn commit_text(&mut self) {
        if self.text_edit.is_null() {
            return;
        }
        let edit = self.text_edit;
        let len = GetWindowTextLengthW(edit);
        let mut buf = vec![0u16; len as usize + 1];
        GetWindowTextW(edit, buf.as_mut_ptr(), len + 1);
        let raw = String::from_utf16_lossy(&buf[..len as usize]);

        // 记下 EDIT 实际用的行高，提交后逐行绘制要用它，否则多行会越写越偏
        let mut tm: TEXTMETRICW = std::mem::zeroed();
        let mut line_h = 0;
        let dc = GetDC(edit);
        if !dc.is_null() {
            let old = SelectObject(dc, self.edit_font as HGDIOBJ);
            GetTextMetricsW(dc, &mut tm);
            SelectObject(dc, old);
            ReleaseDC(edit, dc);
            line_h = tm.tmHeight;
        }
        self.destroy_edit();

        // EDIT 内部用单个 \r 当换行，GDI+ 只认 \n。
        // 注意不能把 \r 直接删掉——那正是换行符本身。
        let text = raw.replace("\r\n", "\n").replace('\r', "\n");
        let text = text.trim_end_matches('\n').to_string();
        if !text.trim().is_empty() {
            self.doc.push(Shape {
                kind: Kind::Text {
                    at: self.text_at,
                    text: text.clone(),
                    line_h,
                },
                style: self.styles[Tool::Text.idx()],
            });
            crate::win::trace(&format!("text: commit {:?}", text));
        }

        SetFocus(self.hwnd);
        InvalidateRect(self.hwnd, std::ptr::null(), 0);
        // 立刻重画一次：复制/保存要从 back buffer 读像素，
        // 晚一帧就会把刚打的字漏掉
        UpdateWindow(self.hwnd);
    }

    /// 放弃这次输入
    unsafe fn cancel_text(&mut self) {
        if self.text_edit.is_null() {
            return;
        }
        crate::win::trace("text: cancel");
        self.destroy_edit();
        SetFocus(self.hwnd);
        InvalidateRect(self.hwnd, std::ptr::null(), 0);
    }

    unsafe fn destroy_edit(&mut self) {
        if !self.text_edit.is_null() {
            RemoveWindowSubclass(self.text_edit, Some(text_edit_proc), 1);
            DestroyWindow(self.text_edit);
            self.text_edit = std::ptr::null_mut();
        }
        if !self.edit_font.is_null() {
            delete_font(self.edit_font);
            self.edit_font = std::ptr::null_mut();
        }
    }

    /// 换行后把输入框撑高，别让字被裁掉
    unsafe fn fit_text_box(&mut self) {
        if self.text_edit.is_null() {
            return;
        }
        let lines = SendMessageW(self.text_edit, EM_GETLINECOUNT, 0, 0) as i32;
        let px = self.styles[Tool::Text.idx()].width.max(10);
        let lh = (px as f32 * 1.4) as i32;
        let h = lines.max(1) * lh + ui(6);
        let mut r: RECT = std::mem::zeroed();
        GetWindowRect(self.text_edit, &mut r);
        SetWindowPos(
            self.text_edit,
            std::ptr::null_mut(),
            0,
            0,
            r.right - r.left,
            h,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }

    fn stroke_begin(&mut self, x: i32, y: i32) {
        let Some(sel) = self.selection() else { return };
        let Some(tool) = self.tool else { return };
        // 文本不是拖出来的，走 EDIT 输入
        if tool == Tool::Text {
            return;
        }
        crate::win::trace(&format!("stroke begin {tool:?} at {x},{y}"));
        let p = POINT {
            x: x - sel.left,
            y: y - sel.top,
        };
        let kind = match tool {
            Tool::Arrow => Kind::Arrow { a: p, b: p },
            Tool::Shape => Kind::Box { a: p, b: p },
            Tool::Pen => Kind::Pen { pts: vec![p] },
            Tool::Text => return,
            Tool::Eraser => Kind::Eraser { pts: vec![p] },
        };
        self.draft = Some(Shape {
            kind,
            style: self.styles[tool.idx()],
        });
        self.drawing = true;
    }

    fn stroke_update(&mut self, x: i32, y: i32) {
        let Some(sel) = self.selection() else { return };
        let p = POINT {
            x: x - sel.left,
            y: y - sel.top,
        };
        let Some(d) = self.draft.as_mut() else { return };
        match &mut d.kind {
            Kind::Arrow { b, .. } | Kind::Box { b, .. } => *b = p,
            Kind::Pen { pts } | Kind::Eraser { pts } => {
                if let Some(last) = pts.last() {
                    if (last.x - p.x).abs() + (last.y - p.y).abs() < 3 {
                        return;
                    }
                }
                pts.push(p);
            }
            _ => {}
        }
    }

    fn stroke_end(&mut self) {
        self.drawing = false;
        if let Some(d) = self.draft.take() {
            let meaningful = match &d.kind {
                Kind::Arrow { a, b } | Kind::Box { a, b } => {
                    (a.x - b.x).abs() + (a.y - b.y).abs() > 3
                }
                Kind::Pen { pts } | Kind::Eraser { pts } => pts.len() > 1,
                _ => false,
            };
            if meaningful {
                self.doc.push(d);
            }
        }
    }

    fn paint(&mut self) {
        unsafe {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(self.hwnd, &mut ps);
            let (w, h) = (self.screen.width, self.screen.height);

            // 1. 底图
            BitBlt(self.back_dc, 0, 0, w, h, self.screen.hdc, 0, 0, SRCCOPY);

            // 2. 蒙层。还没框选时，预测区域要留出来——它应该看起来
            // 和正式选区一样是正常亮度，而不是在蒙层上刷一层颜色。
            let hole = self.selection().or_else(|| {
                if self.dragging {
                    None
                } else {
                    self.predict_client()
                }
            });
            match hole {
                Some(s) => {
                    blend(self.mask_dc, self.back_dc, 0, 0, w, s.top);
                    blend(self.mask_dc, self.back_dc, 0, s.top, s.left, s.bottom);
                    blend(self.mask_dc, self.back_dc, s.right, s.top, w, s.bottom);
                    blend(self.mask_dc, self.back_dc, 0, s.bottom, w, h);
                }
                None => blend(self.mask_dc, self.back_dc, 0, 0, w, h),
            }

            // 预测区域只描一条边：区域本身已经在蒙层里留空了，
            // 再填色反而会盖住内容。按下鼠标（开始拖）就不再画。
            if !self.locked && !self.dragging {
                if let Some(pr) = self.predict_client() {
                    crate::gdiplus::rect_stroke(self.back_dc, &pr, 0xFF2E63, 1.0);
                }
            }

            // 3. 标注元素，裁剪在选区内
            if let Some(sel) = self.selection() {
                let saved = SaveDC(self.back_dc);
                IntersectClipRect(self.back_dc, sel.left, sel.top, sel.right, sel.bottom);
                for s in &self.doc.shapes {
                    match &s.kind {
                        Kind::Eraser { pts } => {
                            erase_stroke(self.back_dc, &self.screen, pts, s.style.width, sel)
                        }
                        _ => edit::draw(self.back_dc, s, sel.left, sel.top),
                    }
                }
                if let Some(d) = &self.draft {
                    match &d.kind {
                        Kind::Eraser { pts } => {
                            erase_stroke(self.back_dc, &self.screen, pts, d.style.width, sel)
                        }
                        _ => edit::draw(self.back_dc, d, sel.left, sel.top),
                    }
                }
                RestoreDC(self.back_dc, saved);
            }

            // 4. 选区边框 + 尺寸标签
            if let Some(s) = self.selection() {
                frame(self.back_dc, &s);
                draw_size_label(self.back_dc, &s, self.label_font);
            }

            // 5. 工具条 + 属性条
            if self.locked && !self.buttons.is_empty() {
                draw_bar(
                    self.back_dc,
                    &self.buttons,
                    self.hover,
                    self.pressed,
                    &self.icons,
                    self.tool,
                );
                if let Some(tool) = self.tool {
                    draw_props(
                        self.back_dc,
                        &self.props,
                        self.hover_prop,
                        &self.styles[tool.idx()],
                        tool,
                    );
                }
            }

            // 6. 预备状态：光标下的像素颜色
            if !self.locked {
                draw_color_hud(
                    self.back_dc,
                    &self.screen,
                    self.cursor,
                    self.hex_mode,
                    self.label_font,
                );
            }

            // 7. 上屏
            BitBlt(hdc, 0, 0, w, h, self.back_dc, 0, 0, SRCCOPY);
            EndPaint(self.hwnd, &ps);
        }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            if !self.back_dc.is_null() {
                DeleteDC(self.back_dc);
            }
            if !self.back_bmp.is_null() {
                DeleteObject(self.back_bmp as HGDIOBJ);
            }
            if !self.mask_dc.is_null() {
                DeleteDC(self.mask_dc);
            }
            if !self.mask_bmp.is_null() {
                DeleteObject(self.mask_bmp as HGDIOBJ);
            }
            delete_font(self.label_font);
        }
    }
}

/// 用 1x1 黑位图拉伸出一块半透明蒙层
unsafe fn blend(mask_dc: HDC, dst: HDC, l: i32, t: i32, r: i32, b: i32) {
    if r <= l || b <= t {
        return;
    }
    let bf = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: MASK_ALPHA,
        AlphaFormat: 0,
    };
    AlphaBlend(dst, l, t, r - l, b - t, mask_dc, 0, 0, 1, 1, bf);
}

/// 圆角填充，无边框。走 GDI+ 拿抗锯齿。
unsafe fn round_fill(dc: HDC, r: &RECT, dia: i32, color: u32) {
    crate::gdiplus::fill_round(dc, r, dia as f32 / 2.0, color);
}

/// 圆角描边，不填充。
unsafe fn round_stroke(dc: HDC, r: &RECT, dia: i32, color: u32) {
    round_stroke_w(dc, r, dia, color, 1);
}

/// 圆角描边，可以指定线宽。同样走 GDI+。
unsafe fn round_stroke_w(dc: HDC, r: &RECT, dia: i32, color: u32, w: i32) {
    crate::gdiplus::stroke_round(dc, r, dia as f32 / 2.0, color, w as f32);
}

/// 选区边框：外面 1px 黑、里面 1px 白。
unsafe fn frame(dc: HDC, s: &RECT) {
    let black = CreatePen(PS_SOLID, 1, rgb(0, 0, 0));
    let white = CreatePen(PS_SOLID, 1, rgb(0xFF, 0xFF, 0xFF));
    let old_pen = SelectObject(dc, black as HGDIOBJ);
    let old_brush = SelectObject(dc, GetStockObject(NULL_BRUSH) as HGDIOBJ);

    Rectangle(dc, s.left - 1, s.top - 1, s.right + 1, s.bottom + 1);
    SelectObject(dc, white as HGDIOBJ);
    Rectangle(dc, s.left, s.top, s.right, s.bottom);

    SelectObject(dc, old_pen);
    SelectObject(dc, old_brush);
    DeleteObject(black as HGDIOBJ);
    DeleteObject(white as HGDIOBJ);
}

unsafe fn draw_size_label(hdc: HDC, s: &RECT, font: HFONT) {
    let text = format!("{} × {}", s.right - s.left, s.bottom - s.top);
    let wide = to_wide(&text);
    let old_f = SelectObject(hdc, font as HGDIOBJ);

    let mut size: windows_sys::Win32::Foundation::SIZE = std::mem::zeroed();
    GetTextExtentPoint32W(hdc, wide.as_ptr(), (wide.len() - 1) as i32, &mut size);

    let (pad_x, pad_y) = (ui(8), ui(4));
    let bw = size.cx + pad_x * 2;
    let bh = size.cy + pad_y * 2;
    let bx = s.left;
    let by = if s.top >= bh + ui(6) {
        s.top - bh - ui(6)
    } else {
        s.top + ui(6)
    };

    let bg = rect(bx, by, bx + bw, by + bh);
    round_fill(hdc, &bg, RADIUS_CHIP, rgb(0x0A, 0x0A, 0x0C));

    SetBkMode(hdc, TRANSPARENT as i32);
    SetTextColor(hdc, rgb(0xFF, 0xFF, 0xFF));
    TextOutW(
        hdc,
        bx + pad_x,
        by + pad_y,
        wide.as_ptr(),
        (wide.len() - 1) as i32,
    );
    SelectObject(hdc, old_f);
}

unsafe fn draw_bar(
    dc: HDC,
    buttons: &[Button],
    hover: i32,
    pressed: i32,
    icons: &IconSet,
    tool: Option<Tool>,
) {
    let bar = rect(
        buttons[0].rect.left,
        buttons[0].rect.top,
        buttons[buttons.len() - 1].rect.right,
        buttons[0].rect.bottom,
    );
    round_fill(dc, &bar, RADIUS_BAR, rgb(0xFF, 0xFF, 0xFF));

    // 工具区 / 操作区之间的分隔线
    let split = 5usize;
    if split < buttons.len() {
        let sep_brush = CreateSolidBrush(rgb(0xD0, 0xD0, 0xD6));
        let x = buttons[split].rect.left;
        let sep = rect(x, bar.top + ui(8), x + 1, bar.bottom - ui(8));
        FillRect(dc, &sep, sep_brush);
        DeleteObject(sep_brush as HGDIOBJ);
    }

    for (i, b) in buttons.iter().enumerate() {
        let idx = i as i32;
        let selected = match (b.act, tool) {
            (Act::Tool(t), Some(cur)) => t == cur,
            _ => false,
        };
        if selected {
            // 选中：只加深底色，不描边。深色描边在白工具栏上太抢眼。
            round_fill(
                dc,
                &rect(
                    b.rect.left + ui(2),
                    b.rect.top + ui(3),
                    b.rect.right - ui(2),
                    b.rect.bottom - ui(3),
                ),
                RADIUS_CHIP,
                SEL_BG,
            );
        } else if hover == idx || pressed == idx {
            round_fill(
                dc,
                &rect(
                    b.rect.left + ui(2),
                    b.rect.top + ui(3),
                    b.rect.right - ui(2),
                    b.rect.bottom - ui(3),
                ),
                RADIUS_CHIP,
                rgb(0xF1, 0xF1, 0xF5),
            );
        }

        let set = if selected {
            &icons.active
        } else {
            &icons.normal
        };
        if let Some(icon) = set.get(b.icon) {
            let ix = (b.rect.left + b.rect.right - crate::icons::SIZE) / 2;
            let iy = (b.rect.top + b.rect.bottom - crate::icons::SIZE) / 2;
            icon.draw(dc, ix, iy);
        }
    }

    round_stroke(dc, &bar, RADIUS_BAR, rgb(0xD6, 0xD6, 0xDC));
}

unsafe fn draw_props(dc: HDC, props: &[(RECT, Prop)], hover_prop: i32, style: &Style, tool: Tool) {
    use windows_sys::Win32::Graphics::Gdi::{LineTo, MoveToEx};

    if props.is_empty() {
        return;
    }
    let bar = rect(
        props[0].0.left,
        props[0].0.top,
        props[props.len() - 1].0.right,
        props[0].0.bottom,
    );
    round_fill(dc, &bar, RADIUS_BAR, rgb(0xFF, 0xFF, 0xFF));

    const ON: (u8, u8, u8) = (0x1A, 0x1A, 0x20);
    const OFF: (u8, u8, u8) = (0x8A, 0x8A, 0x92);

    for (i, (r, prop)) in props.iter().enumerate() {
        let cx = (r.left + r.right) / 2;
        let cy = (r.top + r.bottom) / 2;

        if *prop == Prop::Sep {
            let b = CreateSolidBrush(rgb(0xD8, 0xD8, 0xDE));
            let sep = rect(cx, r.top + ui(6), cx + 1, r.bottom - ui(6));
            FillRect(dc, &sep, b);
            DeleteObject(b as HGDIOBJ);
            continue;
        }

        // 选中态：加深底色（悬停盖在它上面时仍保留选中的深浅差别）
        let selected = match *prop {
            Prop::Color(idx) => style.color == COLORS[idx],
            Prop::Width(idx) => style.width == tool.width_at(idx),
            Prop::Round(is_round) => style.round == is_round,
            _ => false,
        };
        if selected {
            round_fill(
                dc,
                &rect(
                    r.left + ui(2),
                    r.top + ui(3),
                    r.right - ui(2),
                    r.bottom - ui(3),
                ),
                RADIUS_CELL,
                SEL_BG,
            );
        } else if hover_prop == i as i32 {
            round_fill(
                dc,
                &rect(
                    r.left + ui(2),
                    r.top + ui(3),
                    r.right - ui(2),
                    r.bottom - ui(3),
                ),
                RADIUS_CELL,
                rgb(0xF1, 0xF1, 0xF5),
            );
        }

        match *prop {
            Prop::Color(idx) => {
                let d = rect(cx - ui(7), cy - ui(7), cx + ui(7), cy + ui(7));
                round_fill(dc, &d, 8, COLORS[idx]);
                round_stroke(dc, &d, 8, rgb(0xD8, 0xD8, 0xDE));
            }
            Prop::Width(idx) => {
                let sel = style.width == tool.width_at(idx);
                let c = if sel { ON } else { OFF };
                if tool == Tool::Text {
                    // 文本：三档字号用大中小三个 A，比画三根线直观
                    let px = (ui(8) + idx as i32 * ui(3)) as f32;
                    let spec = crate::gdiplus::FontSpec::new(px, false, "Segoe UI");
                    let (w, h) = crate::gdiplus::measure_text(dc, &spec, "A");
                    crate::gdiplus::draw_text(
                        dc,
                        &spec,
                        rgb(c.0, c.1, c.2),
                        cx - w / 2,
                        cy - h / 2,
                        "A",
                    );
                } else {
                    // 三档粗细画成圆头药丸。GDI 直线没有抗锯齿，
                    // 端点会啃出硬台阶，这里交给 GDI+ 画圆角矩形。
                    let shown = (ui(2) + idx as i32 * ui(2)).max(3);
                    let r = rect(cx - ui(7), cy - shown / 2, cx + ui(7), cy + shown / 2);
                    crate::gdiplus::fill_round(dc, &r, shown as f32 / 2.0, rgb(c.0, c.1, c.2));
                }
                let _ = sel;
            }
            Prop::Filled => {
                // 圆角要小：RADIUS_CHIP 在 20px 的框上会变成圆，和"圆角"图标撞车
                let b = rect(cx - ui(6), cy - ui(6), cx + ui(6), cy + ui(6));
                if style.filled {
                    round_fill(dc, &b, 4, rgb(ON.0, ON.1, ON.2));
                } else {
                    round_stroke(dc, &b, 4, rgb(OFF.0, OFF.1, OFF.2));
                }
            }
            Prop::Dashed => {
                let c = if style.dashed { ON } else { OFF };
                let pen = CreatePen(PS_SOLID, ui(2), rgb(c.0, c.1, c.2));
                let old = SelectObject(dc, pen as HGDIOBJ);
                // 始终画成虚线：关闭时只是变灰。原来关闭态画实线，
                // 和旁边三根粗细线长得一样，分不清是哪个控件。
                for k in 0..3 {
                    let x0 = cx - ui(7) + k * ui(5);
                    MoveToEx(dc, x0, cy, std::ptr::null_mut());
                    LineTo(dc, x0 + ui(3), cy);
                }
                SelectObject(dc, old);
                DeleteObject(pen as HGDIOBJ);
            }
            Prop::Bold => {
                let c = if style.bold { ON } else { OFF };
                let spec = crate::gdiplus::FontSpec::new(ui(12) as f32, true, "Segoe UI");
                let (w, h) = crate::gdiplus::measure_text(dc, &spec, "B");
                crate::gdiplus::draw_text(
                    dc,
                    &spec,
                    rgb(c.0, c.1, c.2),
                    cx - w / 2,
                    cy - h / 2,
                    "B",
                );
            }
            Prop::Stroke => {
                let c = if style.stroke { ON } else { OFF };
                let spec = crate::gdiplus::FontSpec::new(ui(12) as f32, true, "Segoe UI");
                let (w, h) = crate::gdiplus::measure_text(dc, &spec, "A");
                crate::gdiplus::text_stroke(
                    dc,
                    &spec,
                    rgb(c.0, c.1, c.2),
                    cx - w / 2,
                    cy - h / 2,
                    "A",
                    1.5,
                );
            }
            Prop::Shadow => {
                let c = if style.shadow { ON } else { OFF };
                let front = rect(cx - ui(5), cy - ui(5), cx + ui(4), cy + ui(4));
                let back = rect(cx - ui(2), cy - ui(2), cx + ui(7), cy + ui(7));
                round_fill(dc, &back, 4, rgb(0xC6, 0xC6, 0xCE));
                round_fill(dc, &front, 4, rgb(c.0, c.1, c.2));
            }
            Prop::Round(is_round) => {
                let sel = style.round == is_round;
                let c = if sel { ON } else { OFF };
                // 圆/方选择器也走 GDI+，小圆的边缘最吃抗锯齿
                let frame = rect(cx - ui(6), cy - ui(6), cx + ui(6), cy + ui(6));
                let col = rgb(c.0, c.1, c.2);
                if is_round {
                    crate::gdiplus::ellipse(dc, &frame, col, ui(2) as f32);
                } else {
                    crate::gdiplus::rect_stroke(dc, &frame, col, ui(2) as f32);
                }
                let _ = sel;
            }
            Prop::Sep => {}
        }
    }

    round_stroke(dc, &bar, RADIUS_BAR, rgb(0xD6, 0xD6, 0xDC));
}

/// 预备截图的取色浮层：色块 + 光标下像素的颜色值。
///
/// 跟着光标走，右下偏移；贴到屏幕边就翻到另一侧，始终完整可见。
unsafe fn draw_color_hud(dc: HDC, screen: &Screen, cursor: POINT, hex: bool, font: HFONT) {
    let Some((r, g, b)) = sample_pixel(screen, cursor.x, cursor.y) else {
        return;
    };
    let text = color_text(r, g, b, hex);
    let wide = to_wide(&text);

    let old_f = SelectObject(dc, font as HGDIOBJ);
    let mut size: windows_sys::Win32::Foundation::SIZE = std::mem::zeroed();
    GetTextExtentPoint32W(dc, wide.as_ptr(), (wide.len() - 1) as i32, &mut size);
    SelectObject(dc, old_f);

    let sw = ui(22);
    let pad = ui(8);
    let gap = ui(8);
    let bw = pad * 2 + sw + gap + size.cx;
    let bh = size.cy.max(sw) + pad * 2;

    let off = ui(18);
    let right = screen.x + screen.width;
    let bottom = screen.y + screen.height;

    let mut bx = cursor.x + off;
    let mut by = cursor.y + off;
    if bx + bw > right {
        bx = cursor.x - off - bw;
    }
    if by + bh > bottom {
        by = cursor.y - off - bh;
    }
    bx = bx.max(screen.x);
    by = by.max(screen.y);

    let bg = rect(bx, by, bx + bw, by + bh);
    round_fill(dc, &bg, RADIUS_CHIP, rgb(0x0A, 0x0A, 0x0C));
    round_stroke(dc, &bg, RADIUS_CHIP, rgb(0x4A, 0x4A, 0x52));

    // 色块就是采样到的颜色本身
    let chip = rect(
        bx + pad,
        by + (bh - sw) / 2,
        bx + pad + sw,
        by + (bh + sw) / 2,
    );
    round_fill(dc, &chip, ui(4), rgb(r, g, b));
    round_stroke(dc, &chip, ui(4), rgb(0x6A, 0x6A, 0x72));

    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(dc, rgb(0xFF, 0xFF, 0xFF));
    let old_f = SelectObject(dc, font as HGDIOBJ);
    TextOutW(
        dc,
        bx + pad + sw + gap,
        by + (bh - size.cy) / 2,
        wide.as_ptr(),
        (wide.len() - 1) as i32,
    );
    SelectObject(dc, old_f);
}

/// 从 back buffer 里取回选区像素 —— 保存/复制要带上标注
unsafe fn read_back_buffer(dc: HDC, sel: RECT) -> Option<image::RgbaImage> {
    use windows_sys::Win32::Graphics::Gdi::{
        GetDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    };

    let w = sel.right - sel.left;
    let h = sel.bottom - sel.top;
    if w <= 0 || h <= 0 {
        return None;
    }

    let screen = GetDC(std::ptr::null_mut());
    let mem = CreateCompatibleDC(screen);
    let bmp = CreateCompatibleBitmap(screen, w, h);
    ReleaseDC(std::ptr::null_mut(), screen);
    if mem.is_null() || bmp.is_null() {
        return None;
    }
    let old = SelectObject(mem, bmp as HGDIOBJ);
    BitBlt(mem, 0, 0, w, h, dc, sel.left, sel.top, SRCCOPY);

    let mut bi: BITMAPINFO = std::mem::zeroed();
    bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = w;
    bi.bmiHeader.biHeight = -h;
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB;

    let mut buf = vec![0u8; (w * h * 4) as usize];
    let lines = GetDIBits(
        mem,
        bmp,
        0,
        h as u32,
        buf.as_mut_ptr() as *mut std::ffi::c_void,
        &mut bi,
        DIB_RGB_COLORS,
    );

    SelectObject(mem, old);
    DeleteObject(bmp as HGDIOBJ);
    DeleteDC(mem);

    if lines == 0 {
        return None;
    }
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2); // BGRA -> RGBA
        px[3] = 255;
    }
    image::RgbaImage::from_raw(w as u32, h as u32, buf)
}

/// 橡皮：把底图对应区域盖回来
unsafe fn erase_stroke(dc: HDC, screen: &Screen, pts: &[POINT], width: i32, sel: RECT) {
    let saved = SaveDC(dc);
    IntersectClipRect(dc, sel.left, sel.top, sel.right, sel.bottom);

    let r = (width * 2).max(6);
    for p in pts {
        let x = p.x + sel.left;
        let y = p.y + sel.top;
        BitBlt(
            dc,
            x - r,
            y - r,
            r * 2,
            r * 2,
            screen.hdc,
            x - r,
            y - r,
            SRCCOPY,
        );
    }
    RestoreDC(dc, saved);
}

// ------------------------------------------------------------- wndproc

extern "system" fn overlay_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let Some(ov) = OVERLAY.as_mut() else {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        };

        match msg {
            WM_MOUSEMOVE => {
                let (x, y) = (lo(lparam), hi(lparam));

                // 正在拖选区：移动或改边，其它逻辑都让路
                if ov.grab != Grab::None {
                    ov.grab_update(x, y);
                    return 0;
                }

                // 预测只在"还没按下鼠标"时更新：一旦开始拖，就不再跟着动了
                if !ov.locked && !ov.dragging {
                    let before = ov.predict;
                    ov.refresh_predict(x, y);
                    let changed = match (before, ov.predict) {
                        (None, None) => false,
                        (Some(a), Some(b)) => {
                            a.left != b.left
                                || a.top != b.top
                                || a.right != b.right
                                || a.bottom != b.bottom
                        }
                        _ => true,
                    };
                    if changed {
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                }

                // 取色浮层跟着光标走，只有还没框选时才需要
                if !ov.locked {
                    let sx = x + ov.screen.x;
                    let sy = y + ov.screen.y;
                    if ov.cursor.x != sx || ov.cursor.y != sy {
                        ov.cursor = POINT { x: sx, y: sy };
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                }

                if ov.drawing {
                    ov.stroke_update(x, y);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                } else if ov.dragging {
                    ov.cur = POINT { x, y };
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                } else if ov.locked {
                    let idx = ov.hit_button(x, y);
                    let pidx = ov.hit_prop(x, y);
                    if idx != ov.hover || pidx != ov.hover_prop {
                        ov.hover = idx;
                        ov.hover_prop = pidx;
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                }
                0
            }
            WM_LBUTTONDOWN => {
                let (x, y) = (lo(lparam), hi(lparam));
                // 点任何地方都先把正在输入的文字落下来
                if !ov.text_edit.is_null() {
                    ov.commit_text();
                }
                let idx = ov.hit_button(x, y);
                let pidx = ov.hit_prop(x, y);

                if idx >= 0 {
                    ov.pressed = idx;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                } else if pidx >= 0 {
                    apply_prop(ov, pidx);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                } else if ov.locked {
                    if let Some(s) = ov.selection() {
                        let zone = ov.edge_zone(&s, x, y);
                        // Ctrl 按住时，即使选了工具也是移动选区（画不了的时候的出口）
                        let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
                        if zone != 0 {
                            ov.begin_grab(x, y, zone);
                            SetCapture(hwnd);
                        } else if pt_in(&s, x, y) {
                            if ov.tool.is_some() && !ctrl {
                                if ov.tool == Some(Tool::Text) {
                                    ov.start_text(x, y);
                                } else {
                                    ov.stroke_begin(x, y);
                                }
                                SetCapture(hwnd);
                            } else {
                                ov.begin_grab(x, y, 0);
                                SetCapture(hwnd);
                            }
                        }
                    }
                } else if !ov.locked {
                    ov.start = POINT { x, y };
                    ov.cur = POINT { x, y };
                    ov.dragging = true;
                    SetCapture(hwnd);
                }
                0
            }
            WM_LBUTTONUP => {
                let (x, y) = (lo(lparam), hi(lparam));
                if ov.grab != Grab::None {
                    ov.end_grab();
                    return 0;
                }
                if ov.drawing {
                    ov.stroke_end();
                    ReleaseCapture();
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                } else if ov.pressed >= 0 {
                    let idx = ov.hit_button(x, y);
                    if idx == ov.pressed && idx >= 0 {
                        let act = ov.buttons[idx as usize].act;
                        ov.pressed = -1;
                        match act {
                            Act::Tool(t) => {
                                ov.tool = if ov.tool == Some(t) { None } else { Some(t) };
                                crate::win::trace(&format!("tool -> {:?}", ov.tool));
                                ov.layout_buttons();
                                InvalidateRect(hwnd, std::ptr::null(), 0);
                            }
                            Act::Copy => ov.finish(CMD_COPY),
                            Act::Save => ov.finish(CMD_SAVE),
                            Act::Pin => ov.finish(CMD_PIN),
                        }
                        return 0;
                    }
                    ov.pressed = -1;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                } else if ov.dragging {
                    ov.cur = POINT { x, y };
                    ov.dragging = false;
                    // 几乎没动 = 单击：采纳 UIA 预测出来的区域
                    let tiny = (ov.cur.x - ov.start.x).abs() + (ov.cur.y - ov.start.y).abs() < 4;
                    if tiny {
                        if let Some(r) = ov.predict.take() {
                            ov.start = POINT {
                                x: r.left,
                                y: r.top,
                            };
                            ov.cur = POINT {
                                x: r.right,
                                y: r.bottom,
                            };
                        }
                    } else {
                        ov.predict = None;
                    }
                    ov.locked = true;
                    ReleaseCapture();
                    ov.layout_buttons();
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                0
            }
            WM_RBUTTONUP => {
                if !ov.text_edit.is_null() {
                    ov.commit_text();
                } else {
                    ov.finish(CMD_CANCEL);
                }
                0
            }
            WM_KEYDOWN => {
                let vk = wparam as u16 as i32;
                let ctrl = GetKeyState(VK_CONTROL as i32) < 0;

                // 预备状态下的取色快捷键
                if !ov.locked && ov.tool.is_none() {
                    if vk == VK_SHIFT as i32 {
                        // 按住 Shift 会连续发 WM_KEYDOWN，lparam 的 bit30 标记重复，
                        // 不过滤的话模式会疯狂来回切
                        if (lparam as usize >> 30) & 1 == 0 {
                            ov.hex_mode = !ov.hex_mode;
                            InvalidateRect(hwnd, std::ptr::null(), 0);
                            crate::win::trace(&format!("pick: hex={}", ov.hex_mode));
                        }
                        return 0;
                    }
                    if vk == VK_C as i32 && !ctrl {
                        if let Some((r, g, b)) = sample_pixel(&ov.screen, ov.cursor.x, ov.cursor.y)
                        {
                            let text = color_text(r, g, b, ov.hex_mode);
                            match crate::clipboard::copy_text(&text) {
                                Ok(()) => crate::win::trace(&format!("pick: copied {text}")),
                                Err(e) => crate::win::trace(&format!("pick: copy failed {e}")),
                            }
                        }
                        ov.finish(CMD_CANCEL);
                        return 0;
                    }
                }

                if vk == VK_ESCAPE as i32 {
                    // 有工具时先退工具，再按一次才关掉整个遮罩
                    if ov.tool.is_some() {
                        ov.tool = None;
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    } else {
                        ov.finish(CMD_CANCEL);
                    }
                    return 0;
                }
                if vk == VK_Z as i32 && ctrl {
                    ov.doc.undo();
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    return 0;
                }
                if vk == VK_RETURN as i32 && ov.tool.is_none() && ov.locked {
                    ov.finish(CMD_COPY);
                    return 0;
                }
                if vk == VK_S as i32 && ctrl && ov.locked {
                    ov.finish(CMD_SAVE);
                    return 0;
                }
                0
            }
            WM_TEXT_COMMIT => {
                ov.commit_text();
                0
            }
            WM_TEXT_CANCEL => {
                ov.cancel_text();
                0
            }
            // 输入框的配色：字色跟着当前设置走，底色反着来
            // 输入框的背景由子类的 WM_ERASEBKGND 从 back buffer 拷过来，
            // 这里只把字色给它、背景刷成空心，看起来就是"透明"的
            WM_CTLCOLOREDIT => {
                let hdc = wparam as HDC;
                let st = ov.styles[Tool::Text.idx()];
                SetTextColor(hdc, st.color);
                SetBkMode(hdc, TRANSPARENT as i32);
                GetStockObject(HOLLOW_BRUSH) as LRESULT
            }
            // 换行后把输入框撑高
            WM_COMMAND => {
                let code = ((wparam >> 16) & 0xFFFF) as u32;
                if !ov.text_edit.is_null() && lparam as HWND == ov.text_edit && code == EN_CHANGE {
                    ov.fit_text_box();
                }
                0
            }
            // 锁定后：边上显示缩放光标，内部显示移动光标
            WM_SETCURSOR => {
                if ov.locked {
                    let mut pt: POINT = std::mem::zeroed();
                    GetCursorPos(&mut pt);
                    let (x, y) = (pt.x - ov.screen.x, pt.y - ov.screen.y);
                    if let Some(s) = ov.selection() {
                        let zone = ov.edge_zone(&s, x, y);
                        if let Some(c) = ov.grab_cursor(zone) {
                            SetCursor(c);
                            return 1;
                        }
                        if zone == 0 && pt_in(&s, x, y) {
                            let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
                            if ov.tool.is_none() || ctrl {
                                SetCursor(ov.cur_all);
                                return 1;
                            }
                        }
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            // 让 UIA 看不见这个窗口：命中测试要穿到遮罩底下，才能预测选区。
            // 返回 0 = 没有 provider，UIA 会跳过它。
            0x003D => {
                crate::win::trace("overlay: WM_GETOBJECT -> 0");
                0
            } // WM_GETOBJECT
            WM_PAINT => {
                ov.paint();
                0
            }
            windows_sys::Win32::UI::WindowsAndMessaging::WM_DESTROY => {
                ov.destroy_edit();
                OVERLAY = None;
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

fn apply_prop(ov: &mut Overlay, idx: i32) {
    let Some(tool) = ov.tool else { return };
    let prop = match ov.props.get(idx as usize) {
        Some((_, p)) => *p,
        None => return,
    };
    crate::win::trace(&format!("prop {prop:?} for {tool:?}"));

    let st = &mut ov.styles[tool.idx()];
    match prop {
        Prop::Color(i) => st.color = COLORS[i.min(COLORS.len() - 1)],
        Prop::Width(i) => st.width = tool.width_at(i),
        Prop::Filled => st.filled = !st.filled,
        Prop::Dashed => st.dashed = !st.dashed,
        Prop::Round(b) => st.round = b,
        Prop::Bold => st.bold = !st.bold,
        Prop::Stroke => st.stroke = !st.stroke,
        Prop::Shadow => st.shadow = !st.shadow,
        Prop::Sep => {}
    }
    save_styles(ov);
}

/// 把五个工具当前的样式写回配置。只改了某一条也要整组存，
/// 这样配置文件里始终是完整的一份。
fn save_styles(ov: &Overlay) {
    let mut cfg = crate::config::Config::load();
    for i in 0..5 {
        cfg.styles[i] = Some(ov.styles[i].to_token());
    }
    if !cfg.save() {
        crate::win::trace("config: 样式保存失败");
    }
}

pub fn register() {
    unsafe {
        let class = to_wide(CLASS_NAME);
        let wc = WNDCLASSW {
            style: 0x0002 | 0x0001, // CS_HREDRAW | CS_VREDRAW
            lpfnWndProc: Some(overlay_wndproc),
            hInstance: hinstance(),
            hCursor: windows_sys::Win32::UI::WindowsAndMessaging::LoadCursorW(
                std::ptr::null_mut(),
                windows_sys::Win32::UI::WindowsAndMessaging::IDC_CROSS,
            ),
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
    }
}

/// 抓屏并弹出遮罩。失败返回 false。
pub fn start() -> bool {
    unsafe {
        if OVERLAY.is_some() {
            crate::win::trace("overlay: already active, skip");
            return false;
        }
        let Some(screen) = capture::capture_desktop() else {
            crate::win::trace("overlay: capture_desktop failed");
            return false;
        };
        let (vx, vy, vw, vh) = capture::desktop_bounds();

        let class = to_wide(CLASS_NAME);
        let title = to_wide("SmartPaste");

        let desktop_dc = GetDC(std::ptr::null_mut());
        let back_dc = CreateCompatibleDC(desktop_dc);
        let back_bmp = CreateCompatibleBitmap(desktop_dc, vw, vh);
        SelectObject(back_dc, back_bmp as HGDIOBJ);

        let mask_dc = CreateCompatibleDC(desktop_dc);
        let mask_bmp = CreateCompatibleBitmap(desktop_dc, 1, 1);
        SelectObject(mask_dc, mask_bmp as HGDIOBJ);
        let black = CreateSolidBrush(rgb(0, 0, 0));
        let one = rect(0, 0, 1, 1);
        FillRect(mask_dc, &one, black);
        DeleteObject(black as HGDIOBJ);
        ReleaseDC(std::ptr::null_mut(), desktop_dc);

        // 遮罩出现前的前台窗口，就是用户最想截的那个
        let prev_fg = GetForegroundWindow();
        OVERLAY = Some(Box::new(Overlay {
            hwnd: std::ptr::null_mut(),
            screen,
            dragging: false,
            locked: false,
            start: POINT { x: 0, y: 0 },
            cur: POINT { x: 0, y: 0 },
            hover: -1,
            hover_prop: -1,
            pressed: -1,
            buttons: Vec::new(),
            props: Vec::new(),
            tool: None,
            cursor: {
                let mut pt: POINT = std::mem::zeroed();
                GetCursorPos(&mut pt);
                pt
            },
            hex_mode: false,
            styles: {
                let base = Style::default();
                let mut arrow = base;
                arrow.width = 4;
                let mut eraser = base;
                eraser.width = 24;
                eraser.round = true;
                let mut text = base;
                text.width = 26;
                [base, arrow, base, text, eraser]
            },
            draft: None,
            drawing: false,
            predict: None,
            prev_fg,
            grab: Grab::None,
            grab_rect: rect(0, 0, 0, 0),
            grab_from: POINT { x: 0, y: 0 },
            cur_we: LoadCursorW(std::ptr::null_mut(), IDC_SIZEWE),
            cur_ns: LoadCursorW(std::ptr::null_mut(), IDC_SIZENS),
            cur_nwse: LoadCursorW(std::ptr::null_mut(), IDC_SIZENWSE),
            cur_nesw: LoadCursorW(std::ptr::null_mut(), IDC_SIZENESW),
            cur_all: LoadCursorW(std::ptr::null_mut(), IDC_SIZEALL),
            doc: Doc::default(),
            text_edit: std::ptr::null_mut(),
            text_at: POINT { x: 0, y: 0 },
            edit_font: std::ptr::null_mut(),
            icons: crate::icons::load(),
            back_dc,
            back_bmp,
            mask_dc,
            mask_bmp,
            label_font: ui_font(ui(12)),
        }));

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP | WS_VISIBLE,
            vx,
            vy,
            vw,
            vh,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance(),
            std::ptr::null_mut(),
        );

        if hwnd.is_null() {
            OVERLAY = None;
            crate::win::trace("overlay: CreateWindow failed");
            return false;
        }

        if let Some(ov) = OVERLAY.as_mut() {
            ov.hwnd = hwnd;
            // 每个工具上次用的颜色/线宽，恢复回来
            let cfg = crate::config::Config::load();
            for i in 0..5 {
                if let Some(tok) = &cfg.styles[i] {
                    if let Some(st) = crate::edit::Style::from_token(tok) {
                        ov.styles[i] = st;
                    }
                }
            }
        }
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        SetFocus(hwnd);
        // 一打开就按当前光标位置算一次，不用先动鼠标才看得到预测
        let mut cp: POINT = std::mem::zeroed();
        GetCursorPos(&mut cp);
        if let Some(ov) = OVERLAY.as_mut() {
            ov.refresh_predict(cp.x - vx, cp.y - vy);
        }
        crate::win::trace(&format!("overlay: started {vw}x{vh} at ({vx},{vy})"));

        if let Some((px, py, pw, ph)) = PRESET {
            PRESET = None;
            if let Some(ov) = OVERLAY.as_mut() {
                ov.start = POINT { x: px, y: py };
                ov.cur = POINT {
                    x: px + pw,
                    y: py + ph,
                };
                ov.locked = true;
                ov.layout_buttons();
                InvalidateRect(hwnd, std::ptr::null(), 0);
                crate::win::trace(&format!("overlay: preset ({px},{py},{pw},{ph})"));
            }
        }
        true
    }
}

pub fn take_pending() -> (u32, Option<image::RgbaImage>, POINT) {
    unsafe {
        let cmd = PENDING_CMD;
        PENDING_CMD = CMD_NONE;
        (cmd, PENDING_IMAGE.take(), PENDING_ORIGIN)
    }
}

/// 销毁遮罩窗口。必须从遮罩的 WndProc 之外调用。
pub fn close() {
    unsafe {
        let hwnd = match OVERLAY.as_ref() {
            Some(ov) => ov.hwnd,
            None => return,
        };
        DestroyWindow(hwnd);
    }
}
