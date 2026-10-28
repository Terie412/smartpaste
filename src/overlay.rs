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
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows_sys::Win32::Graphics::Gdi::{
    AlphaBlend, BeginPaint, BitBlt, CreateBitmap, CreateCompatibleBitmap, CreateCompatibleDC,
    CreateDIBSection, CreatePen, CreateSolidBrush, DeleteDC, DeleteObject, EndPaint, FillRect,
    GdiFlush, GetDC, GetStockObject, GetTextExtentPoint32W, GetTextMetricsW, IntersectClipRect,
    InvalidateRect, Rectangle, ReleaseDC, RestoreDC, SaveDC, ScreenToClient, SelectObject,
    SetBkMode, SetStretchBltMode, SetTextColor, StretchBlt, TextOutW, UpdateWindow, AC_SRC_OVER,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, COLORONCOLOR, DIB_RGB_COLORS, HDC, HFONT,
    HGDIOBJ, HOLLOW_BRUSH, NULL_BRUSH, PAINTSTRUCT, PS_SOLID, SRCCOPY, TEXTMETRICW, TRANSPARENT,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_C, VK_CONTROL,
    VK_ESCAPE, VK_RETURN, VK_S, VK_SHIFT, VK_Z,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    ChildWindowFromPointEx, CreateIconIndirect, CreateWindowExW, DefWindowProcW, DestroyWindow,
    GetClientRect, GetCursorPos, GetForegroundWindow, GetSystemMetrics, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, KillTimer, LoadCursorW, PostMessageW, RegisterClassW,
    SendMessageW, SetCursor, SetForegroundWindow, SetTimer, SetWindowPos, SetWindowTextW,
    ShowWindow,
    CWP_SKIPINVISIBLE,
    CWP_SKIPTRANSPARENT, EC_LEFTMARGIN, EC_RIGHTMARGIN, EN_CHANGE, ES_MULTILINE, ES_NOHIDESEL,
    ES_WANTRETURN, HCURSOR, ICONINFO, IDC_ARROW, IDC_CROSS, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS,
    IDC_SIZENWSE, IDC_SIZEWE, SM_CXCURSOR, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SW_SHOW,
    WM_APP, WM_COMMAND, WM_CTLCOLOREDIT, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_PAINT, WM_RBUTTONUP, WM_SETCURSOR, WM_TIMER, WNDCLASSW, WS_CHILD,
    WS_EX_TOOLWINDOW,
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
pub const CMD_RECORD: u32 = 5;

/// 倒计时定时器。用 SetTimer 而不是自己开线程，遮罩本来就有消息循环。
const TIMER_COUNTDOWN: usize = 0x51;

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
/// 取色面板的 Hex 输入：回车落定 / Esc 还原
const WM_HEX_COMMIT: u32 = WM_APP + 7;
const WM_HEX_CANCEL: u32 = WM_APP + 8;

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

// ---- 取色面板 ----

/// 面板内边距
const PK_PAD: i32 = ui(10);
/// 面板内元素之间的竖直间距
const PK_GAP: i32 = ui(9);
/// 饱和度/明度方块
const PK_SV_W: i32 = ui(150);
const PK_SV_H: i32 = ui(80);
/// 色相条
const PK_HUE_H: i32 = ui(12);
/// Hex 输入框
const PK_FIELD_H: i32 = ui(24);
/// 色格的间距（边长按列宽现算，保证正好铺满）
const PK_CHIP_GAP: i32 = ui(4);
/// 小标题（"最近""预设"）的高度
const PK_LABEL_H: i32 = ui(14);
/// 预设色列数
const PK_COLS: i32 = 6;
/// 最近使用色的上限，同时也是面板里那一行的格子数
const RECENT_MAX: usize = 8;

/// `#RRGGBB` → COLORREF。const 上下文里要能算，所以不用函数式构造。
const fn cref(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

/// 面板里的预设色：第一行就是属性条上那 6 个，下面两行是深一档和浅一档。
const PRESETS: [u32; 18] = [
    cref(0xFF, 0x2E, 0x63),
    cref(0xFF, 0xC8, 0x00),
    cref(0x00, 0x8F, 0xFF),
    cref(0x50, 0xC8, 0x78),
    cref(0xFF, 0xFF, 0xFF),
    cref(0x00, 0x00, 0x00),
    cref(0xD8, 0x1E, 0x48),
    cref(0xD9, 0xA4, 0x00),
    cref(0x00, 0x70, 0xCC),
    cref(0x2E, 0x8B, 0x57),
    cref(0x8A, 0x8A, 0x92),
    cref(0xF1, 0xF1, 0xF5),
    cref(0xFF, 0x7A, 0x9C),
    cref(0xFF, 0xDB, 0x4D),
    cref(0x6F, 0xC3, 0xFF),
    cref(0x9F, 0xE0, 0xB4),
    cref(0xC9, 0xC9, 0xD1),
    cref(0x3A, 0x3A, 0x44),
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

        static SHAPE: [Prop; 14] = [
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
            Prop::Custom,
            Prop::Sep,
            Prop::Filled,
            Prop::Sep,
            Prop::Width,
        ];
        static ARROW: [Prop; 11] = [
            // 颜色 | 线型 | 粗细
            Prop::Color(0),
            Prop::Color(1),
            Prop::Color(2),
            Prop::Color(3),
            Prop::Color(4),
            Prop::Color(5),
            Prop::Custom,
            Prop::Sep,
            Prop::Dashed,
            Prop::Sep,
            Prop::Width,
        ];
        static PEN: [Prop; 9] = [
            // 颜色 | 粗细
            Prop::Color(0),
            Prop::Color(1),
            Prop::Color(2),
            Prop::Color(3),
            Prop::Color(4),
            Prop::Color(5),
            Prop::Custom,
            Prop::Sep,
            Prop::Width,
        ];
        static TEXT: [Prop; 13] = [
            // 颜色 | 字号 | 字效
            Prop::Color(0),
            Prop::Color(1),
            Prop::Color(2),
            Prop::Color(3),
            Prop::Color(4),
            Prop::Color(5),
            Prop::Custom,
            Prop::Sep,
            Prop::Width,
            Prop::Sep,
            Prop::Bold,
            Prop::Stroke,
            Prop::Shadow,
        ];
        static ERASER: [Prop; 4] = [
            // 粗细 | 形状
            Prop::Width,
            Prop::Sep,
            Prop::Round(false),
            Prop::Round(true),
        ];

        let _ = COLORS_6;
        match self {
            Tool::Shape => &SHAPE,
            Tool::Arrow => &ARROW,
            Tool::Pen => &PEN,
            Tool::Text => &TEXT,
            Tool::Eraser => &ERASER,
        }
    }

    /// 笔触档位因工具而异：橡皮明显要粗得多，文本这一档是字号
    /// 这个工具线宽/字号的可用区间。滚轮按 1 步调，所以区间要够宽。
    fn width_range(self) -> (i32, i32) {
        match self {
            Tool::Text => (10, 72),
            Tool::Eraser => (6, 80),
            _ => (1, 24),
        }
    }
}

/// 放大镜的边长（像素格数）
const LENS_N: i32 = 15;

/// 属性条上的一格
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Prop {
    Color(usize),
    /// 打开取色面板（连续取色 + 预设色表 + Hex）
    Custom,
    Width,
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
    Record,
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

/// 取色面板。几何一次算好存下来，绘制和命中都读它，不重复算。
///
/// HSV 三个分量单独存，不靠 RGB 反推：在 v=0 / s=0 这种边界上 RGB 会丢信息，
/// 拖回原位色相就跳了。
struct Picker {
    rect: RECT,
    sv: RECT,
    hue: RECT,
    field: RECT,
    dropper: RECT,
    /// 预设色格
    presets: Vec<(RECT, u32)>,
    /// 最近使用那一行。颜色每次从 Overlay::recent 现取，位置固定
    recent_rects: Vec<RECT>,
    /// 0 空闲，1 拖 SV，2 拖色相
    drag: u8,
    h: f32,
    s: f32,
    v: f32,
    /// 悬停的格子序号：0..RECENT_MAX 是最近使用，之后是预设；-1 无
    hover: i32,
    hover_drop: bool,
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
    /// 放大镜：9x9 的 DIB，逐像素填好后 StretchBlt 放大（COLORONCOLOR 就是最近邻）
    lens_dc: HDC,
    #[allow(dead_code)]
    lens_bmp: windows_sys::Win32::Graphics::Gdi::HBITMAP,
    lens_bits: *mut u32,
    mask_dc: HDC,
    mask_bmp: windows_sys::Win32::Graphics::Gdi::HBITMAP,
    label_font: HFONT,
    /// 录制倒计时剩余秒数，0 表示没在倒计时
    pub countdown: i32,
    count_font: HFONT,
    /// 取色面板，None = 没开
    picker: Option<Picker>,
    /// 面板里的 Hex 输入框（独立的 EDIT，和文字标注那个不是一套）
    hex_edit: HWND,
    hex_font: HFONT,
    /// 正在程序化改写 Hex 文本，期间忽略 EN_CHANGE，否则自己触发自己
    hex_sync: bool,
    /// HSV 渐变缓存的画布（SV 方块 + 色相条上下拼在一起）
    grad_dc: HDC,
    #[allow(dead_code)]
    grad_bmp: windows_sys::Win32::Graphics::Gdi::HBITMAP,
    grad_bits: *mut u32,
    /// 缓存对应哪个色相，-1 = 脏
    grad_hue: f32,
    /// 最近用过的颜色（COLORREF），新的在前
    recent: Vec<u32>,
    /// 吸管模式：下一次点击把光标下的像素作为当前颜色
    eyedrop: bool,
}

static mut OVERLAY: Option<Box<Overlay>> = None;
/// 等待主窗口处理的命令
static mut PENDING_CMD: u32 = CMD_NONE;
/// 裁剪结果（交给主窗口做复制/保存/钉图）
static mut PENDING_IMAGE: Option<image::RgbaImage> = None;
static mut PENDING_ORIGIN: POINT = POINT { x: 0, y: 0 };
/// 录制用的选区（屏幕物理像素）
static mut PENDING_RECT: RECT = RECT { left: 0, top: 0, right: 0, bottom: 0 };
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

/// HSV → COLORREF。h 取 0..360，s/v 取 0..1。
fn hsv_rgb(h: f32, s: f32, v: f32) -> u32 {
    let h = h.rem_euclid(360.0);
    let s = s.clamp(0.0, 1.0);
    let v = v.clamp(0.0, 1.0);
    let c = v * s;
    let hp = h / 60.0;
    let x = c * (1.0 - ((hp % 2.0) - 1.0).abs());
    let (r, g, b) = match hp as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let to = |f: f32| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    cref(to(r), to(g), to(b))
}

/// COLORREF → HSV
fn rgb_hsv(color: u32) -> (f32, f32, f32) {
    let r = (color & 0xFF) as f32 / 255.0;
    let g = ((color >> 8) & 0xFF) as f32 / 255.0;
    let b = ((color >> 16) & 0xFF) as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = if d <= f32::EPSILON {
        0.0
    } else if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (h.rem_euclid(360.0), if max <= 0.0 { 0.0 } else { d / max }, max)
}

/// COLORREF（0x00BBGGRR）→ 32bpp DIB 里一个像素该有的值（0x00RRGGBB）。
/// 凡是手工往 DIB 里填像素的地方都得过这一下，写反了红蓝就换过来。
fn dib_px(c: u32) -> u32 {
    ((c & 0xFF) << 16) | (c & 0x00FF_00) | ((c >> 16) & 0xFF)
}

fn hex_text(color: u32) -> String {    format!(
        "#{:02X}{:02X}{:02X}",
        color & 0xFF,
        (color >> 8) & 0xFF,
        (color >> 16) & 0xFF
    )
}

/// 宽容地解析 `#RRGGBB`：允许没有 `#`、允许大小写、忽略夹在里面的空格。
/// 位数不对就返回 None，让输入框可以处在"打到一半"的状态。
fn parse_hex(s: &str) -> Option<u32> {
    let mut v = 0u32;
    let mut n = 0;
    for c in s.chars() {
        if let Some(d) = c.to_digit(16) {
            v = v * 16 + d;
            n += 1;
            if n > 6 {
                return None;
            }
        } else if c.is_whitespace() || c == '#' {
            continue;
        } else {
            return None;
        }
    }
    if n != 6 {
        return None;
    }
    Some(cref(
        ((v >> 16) & 0xFF) as u8,
        ((v >> 8) & 0xFF) as u8,
        (v & 0xFF) as u8,
    ))
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

/// 面板里 Hex 输入框的子类过程。
///
/// 和文字标注那个 EDIT 是两套：这里只需要"回车落定 / Esc 还原"，
/// 而且底层是面板卡片，擦背景同样从 back buffer 拷。
unsafe extern "system" fn hex_edit_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if msg == WM_KEYDOWN {
        let vk = wparam as u32;
        if vk == VK_RETURN as u32 {
            if let Some(ov) = OVERLAY.as_ref() {
                PostMessageW(ov.hwnd, WM_HEX_COMMIT, 0, 0);
            }
            return 0;
        }
        if vk == VK_ESCAPE as u32 {
            if let Some(ov) = OVERLAY.as_ref() {
                PostMessageW(ov.hwnd, WM_HEX_CANCEL, 0, 0);
            }
            return 0;
        }
    }
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
            (Act::Record, 9),
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
        // 选区一动，面板的锚点就作废了，先收起来
        self.close_picker();
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
        // 录制不截图、不标注，只需要把选区范围交出去，省掉一次整块裁剪
        if cmd == CMD_RECORD {
            unsafe {
                KillTimer(self.hwnd, TIMER_COUNTDOWN);
                self.countdown = 0;
                if let Some(sel) = self.selection() {
                    PENDING_RECT = rect(
                        self.screen.x + sel.left,
                        self.screen.y + sel.top,
                        self.screen.x + sel.right,
                        self.screen.y + sel.bottom,
                    );
                }
                PENDING_CMD = cmd;
                crate::win::trace(&format!(
                    "overlay: record sel=({},{},{},{})",
                    PENDING_RECT.left, PENDING_RECT.top, PENDING_RECT.right, PENDING_RECT.bottom
                ));
                if !MAIN_HWND.is_null() {
                    PostMessageW(MAIN_HWND, WM_SNAP_DONE, cmd as usize, 0);
                } else {
                    DestroyWindow(self.hwnd);
                }
            }
            return;
        }
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
            // 保存走"另存为"：每次都让用户决定存哪儿，只在成功后记住目录。
            // 取消就停在遮罩里，选区还在，可以接着改或换个区域。
            if cmd == CMD_SAVE {
                let cfg = crate::config::Config::load();
                let name = crate::savedlg::default_name();
                let Some(path) = crate::savedlg::ask(self.hwnd, cfg.save_dir.as_deref(), &name)
                else {
                    crate::win::trace("save: 用户取消");
                    return;
                };
                if img.save(&path).is_err() {
                    crate::win::trace(&format!("save: 写入失败 {}", path.display()));
                    return;
                }
                let mut cfg = cfg;
                cfg.save_dir = path
                    .parent()
                    .map(|d| d.display().to_string())
                    .filter(|d| !d.is_empty());
                cfg.save();
                crate::win::trace(&format!("saved {}", path.display()));
            } else {
                unsafe {
                    PENDING_IMAGE = Some(img);
                    PENDING_ORIGIN = POINT {
                        x: self.screen.x + sel.left,
                        y: self.screen.y + sel.top,
                    };
                }
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

    // ------------------------------------------------------------ 取色面板

    /// 打开取色面板。已经开着就当作"再点一次关闭"。
    ///
    /// 几何在这里一次算死：颜色格按列数均分内容宽度，所以右边缘天然对齐；
    /// 位置优先贴在属性条上方，放不下翻到下方，最后夹进屏幕。
    unsafe fn open_picker(&mut self) {
        if self.tool.is_none() {
            return;
        }
        if self.picker.is_some() {
            self.close_picker();
            return;
        }
        self.commit_text();

        let Some(anchor) = self
            .props
            .iter()
            .find(|(_, p)| *p == Prop::Custom)
            .map(|(r, _)| *r)
        else {
            return;
        };

        let cw = PK_SV_W;
        let chip = (cw - (PK_COLS - 1) * PK_CHIP_GAP) / PK_COLS;
        let rows = PRESETS.len() as i32 / PK_COLS;
        let small = (cw - (RECENT_MAX as i32 - 1) * PK_CHIP_GAP) / RECENT_MAX as i32;

        let content_h = PK_SV_H
            + PK_GAP
            + PK_HUE_H
            + PK_GAP
            + PK_FIELD_H
            + PK_GAP
            + PK_LABEL_H
            + small
            + PK_GAP
            + PK_LABEL_H
            + rows * chip
            + (rows - 1) * PK_CHIP_GAP;
        let w = cw + PK_PAD * 2;
        let h = content_h + PK_PAD * 2;

        let mut left = anchor.right - w;
        left = left.clamp(4, (self.screen.width - w - 4).max(4));
        let mut top = anchor.top - PK_GAP - h;
        if top < 4 {
            top = anchor.bottom + PK_GAP;
            if top + h > self.screen.height - 4 {
                top = (self.screen.height - h - 4).max(4);
            }
        }

        let card = rect(left, top, left + w, top + h);
        let x0 = left + PK_PAD;
        let mut y = top + PK_PAD;
        let sv = rect(x0, y, x0 + cw, y + PK_SV_H);
        y += PK_SV_H + PK_GAP;
        let hue = rect(x0, y, x0 + cw, y + PK_HUE_H);
        y += PK_HUE_H + PK_GAP;
        let field = rect(x0, y, x0 + cw - PK_FIELD_H - PK_GAP, y + PK_FIELD_H);
        let dropper = rect(x0 + cw - PK_FIELD_H, y, x0 + cw, y + PK_FIELD_H);
        y += PK_FIELD_H + PK_GAP + PK_LABEL_H;

        let mut recent_rects = Vec::with_capacity(RECENT_MAX);
        for i in 0..RECENT_MAX as i32 {
            let cx = x0 + i * (small + PK_CHIP_GAP);
            recent_rects.push(rect(cx, y, cx + small, y + small));
        }
        y += small + PK_GAP + PK_LABEL_H;

        let mut presets = Vec::with_capacity(PRESETS.len());
        for (i, c) in PRESETS.iter().enumerate() {
            let (col, row) = (i as i32 % PK_COLS, i as i32 / PK_COLS);
            let cx = x0 + col * (chip + PK_CHIP_GAP);
            let cy = y + row * (chip + PK_CHIP_GAP);
            presets.push((rect(cx, cy, cx + chip, cy + chip), *c));
        }

        let color = self.styles[self.tool.unwrap().idx()].color;
        let (hh, ss, vv) = rgb_hsv(color);
        self.picker = Some(Picker {
            rect: card,
            sv,
            hue,
            field,
            dropper,
            presets,
            recent_rects,
            drag: 0,
            h: hh,
            s: ss,
            v: vv,
            hover: -1,
            hover_drop: false,
        });
        self.grad_hue = -1.0;
        self.create_hex_edit(field);
        crate::win::trace(&format!(
            "picker: open {}x{} at ({left},{top})",
            w, h
        ));
    }

    unsafe fn close_picker(&mut self) {
        if self.picker.is_none() && self.hex_edit.is_null() {
            return;
        }
        self.destroy_hex_edit();
        self.picker = None;
        self.grad_hue = -1.0;
        // 收尾统一落一次盘：Hex 输入和拖动途中都是攒着不写的
        if let Some(tool) = self.tool {
            let c = self.styles[tool.idx()].color;
            self.push_recent(c);
            save_styles(self);
        }
        crate::win::trace("picker: close");
        if !self.hwnd.is_null() {
            SetFocus(self.hwnd);
            InvalidateRect(self.hwnd, std::ptr::null(), 0);
        }
    }

    /// Hex 输入框：和文字标注那套 EDIT 分开，换行/字号/字体都不一样
    unsafe fn create_hex_edit(&mut self, field: RECT) {
        self.destroy_hex_edit();
        let font = ui_font_w(ui(12), 600);
        let edit = CreateWindowExW(
            0,
            to_wide("EDIT").as_ptr(),
            std::ptr::null(),
            WS_CHILD | WS_VISIBLE,
            field.left + ui(6),
            field.top + ui(6),
            field.right - field.left - ui(12),
            field.bottom - field.top - ui(12),
            self.hwnd,
            2 as *mut core::ffi::c_void,
            hinstance(),
            std::ptr::null_mut(),
        );
        if edit.is_null() {
            crate::win::trace("picker: hex EDIT failed");
            delete_font(font);
            return;
        }
        self.hex_edit = edit;
        self.hex_font = font;
        SendMessageW(edit, WM_SETFONT, font as WPARAM, 1);
        SendMessageW(
            edit,
            EM_SETMARGINS,
            (EC_LEFTMARGIN | EC_RIGHTMARGIN) as WPARAM,
            0,
        );
        SetWindowSubclass(edit, Some(hex_edit_proc), 2, 0);
        self.sync_hex_text();
    }

    unsafe fn destroy_hex_edit(&mut self) {
        if !self.hex_edit.is_null() {
            RemoveWindowSubclass(self.hex_edit, Some(hex_edit_proc), 2);
            DestroyWindow(self.hex_edit);
            self.hex_edit = std::ptr::null_mut();
        }
        if !self.hex_font.is_null() {
            delete_font(self.hex_font);
            self.hex_font = std::ptr::null_mut();
        }
    }

    /// 把当前颜色写进 Hex 框。`hex_sync` 挡住由此触发的 EN_CHANGE，否则自己咬自己。
    unsafe fn sync_hex_text(&mut self) {
        if self.hex_edit.is_null() {
            return;
        }
        let Some(tool) = self.tool else { return };
        let s = to_wide(&hex_text(self.styles[tool.idx()].color));
        self.hex_sync = true;
        SetWindowTextW(self.hex_edit, s.as_ptr());
        self.hex_sync = false;
    }

    /// Hex 框回车：解析成功就落定，失败就还原成当前颜色
    unsafe fn commit_hex(&mut self) {
        let raw = self.hex_raw();
        match parse_hex(&raw) {
            Some(c) => self.set_color(c, true),
            None => self.sync_hex_text(),
        }
        SetFocus(self.hwnd);
    }

    /// 边打边生效。不整串重写输入框——那会把光标推回开头。
    unsafe fn hex_live(&mut self) {
        let raw = self.hex_raw();
        let Some(c) = parse_hex(&raw) else { return };
        let (h, s, v) = rgb_hsv(c);
        if let Some(pk) = self.picker.as_mut() {
            // 纯灰没有色相，保留原来的，否则指针会跳回红
            if s > 0.0 {
                pk.h = h;
            }
            pk.s = s;
            pk.v = v;
        }
        if let Some(tool) = self.tool {
            self.styles[tool.idx()].color = c;
        }
        InvalidateRect(self.hwnd, std::ptr::null(), 0);
    }

    unsafe fn hex_raw(&self) -> String {
        if self.hex_edit.is_null() {
            return String::new();
        }
        let len = GetWindowTextLengthW(self.hex_edit);
        let mut buf = vec![0u16; len as usize + 1];
        GetWindowTextW(self.hex_edit, buf.as_mut_ptr(), len + 1);
        String::from_utf16_lossy(&buf[..len as usize])
    }

    /// 改当前工具的颜色。`remember = false` 用于拖动过程中的连续更新——
    /// 那时不写配置也不记最近使用，抬手时才落一次。
    unsafe fn set_color(&mut self, color: u32, remember: bool) {
        let Some(tool) = self.tool else { return };
        self.styles[tool.idx()].color = color;
        if remember {
            self.push_recent(color);
            save_styles(self);
        }
        self.sync_hex_text();
    }

    fn push_recent(&mut self, color: u32) {
        self.recent.retain(|c| *c != color);
        self.recent.insert(0, color);
        self.recent.truncate(RECENT_MAX);
    }

    /// 点到了哪个颜色格。返回 (颜色, 格子序号)；空格子返回 None。
    fn picker_color_at(&self, x: i32, y: i32) -> Option<(u32, i32)> {
        let pk = self.picker.as_ref()?;
        for (i, r) in pk.recent_rects.iter().enumerate() {
            if pt_in(r, x, y) {
                return self.recent.get(i).map(|c| (*c, i as i32));
            }
        }
        for (i, (r, c)) in pk.presets.iter().enumerate() {
            if pt_in(r, x, y) {
                return Some((*c, (RECENT_MAX + i) as i32));
            }
        }
        None
    }

    unsafe fn picker_click(&mut self, x: i32, y: i32) {
        let Some(pk) = self.picker.as_ref() else { return };
        let (inside, sv, hue, field, dropper) =
            (pt_in(&pk.rect, x, y), pk.sv, pk.hue, pk.field, pk.dropper);

        if !inside {
            crate::win::trace(&format!("picker: click outside ({x},{y}) -> close"));
            self.close_picker();
            return;
        }
        // 点了面板上别的地方就把焦点从 Hex 框收回来，
        // 否则后面的 Esc 会被 Hex 框吃掉（只还原文字，不关面板）
        if !pt_in(&field, x, y) {
            SetFocus(self.hwnd);
        }
        if let Some((c, _)) = self.picker_color_at(x, y) {
            self.set_color(c, true);
            InvalidateRect(self.hwnd, std::ptr::null(), 0);
            return;
        }
        if pt_in(&sv, x, y) {
            if let Some(p) = self.picker.as_mut() {
                p.drag = 1;
            }
            SetFocus(self.hwnd);
            SetCapture(self.hwnd);
            self.picker_drag(x, y);
            return;
        }
        if pt_in(&hue, x, y) {
            if let Some(p) = self.picker.as_mut() {
                p.drag = 2;
            }
            SetFocus(self.hwnd);
            SetCapture(self.hwnd);
            self.picker_drag(x, y);
            return;
        }
        if pt_in(&field, x, y) {
            if !self.hex_edit.is_null() {
                SetFocus(self.hex_edit);
            }
            return;
        }
        if pt_in(&dropper, x, y) {
            self.close_picker();
            self.eyedrop = true;
            InvalidateRect(self.hwnd, std::ptr::null(), 0);
        }
    }

    /// 拖动 SV 方块 / 色相条
    unsafe fn picker_drag(&mut self, x: i32, y: i32) {
        let Some(pk) = self.picker.as_mut() else { return };
        let (fx, fy) = (
            (pk.sv.right - pk.sv.left - 1).max(1) as f32,
            (pk.sv.bottom - pk.sv.top - 1).max(1) as f32,
        );
        match pk.drag {
            1 => {
                pk.s = ((x - pk.sv.left) as f32 / fx).clamp(0.0, 1.0);
                pk.v = 1.0 - ((y - pk.sv.top) as f32 / fy).clamp(0.0, 1.0);
            }
            2 => {
                let fw = (pk.hue.right - pk.hue.left - 1).max(1) as f32;
                pk.h = 360.0 * ((x - pk.hue.left) as f32 / fw).clamp(0.0, 1.0);
            }
            _ => return,
        }
        let color = hsv_rgb(pk.h, pk.s, pk.v);
        self.set_color(color, false);
        InvalidateRect(self.hwnd, std::ptr::null(), 0);
    }

    /// 面板内的悬停。有变化返回 true（调用方据此决定要不要重画）。
    unsafe fn picker_hover(&mut self, x: i32, y: i32) -> bool {
        let idx = self
            .picker_color_at(x, y)
            .map(|(_, i)| i)
            .unwrap_or(-1);
        let drop = match self.picker.as_ref() {
            Some(pk) => pt_in(&pk.dropper, x, y),
            None => false,
        };
        let changed = match self.picker.as_ref() {
            Some(pk) => pk.hover != idx || pk.hover_drop != drop,
            None => false,
        };
        if changed {
            if let Some(pk) = self.picker.as_mut() {
                pk.hover = idx;
                pk.hover_drop = drop;
            }
        }
        changed
    }

    /// HSV 渐变缓存：只有色相变了才重算。SV 方块和色相条拼在同一张 DIB 上，
    /// 竖直方向 0..PK_SV_H 是方块，PK_SV_H.. 是色相条。
    unsafe fn ensure_gradient(&mut self, h: f32) {
        if (self.grad_hue - h).abs() < 0.001 || self.grad_bits.is_null() {
            return;
        }
        let w = PK_SV_W;
        for y in 0..PK_SV_H {
            let v = 1.0 - y as f32 / (PK_SV_H - 1).max(1) as f32;
            for x in 0..w {
                let s = x as f32 / (w - 1).max(1) as f32;
                // hsv_rgb 给的是 COLORREF（0x00BBGGRR），DIB 里要 0x00RRGGBB，
                // 红蓝必须换回来——不换的话整块渐变会从粉红变成蓝紫
                *self.grad_bits.add((y * w + x) as usize) = dib_px(hsv_rgb(h, s, v));
            }
        }
        for y in 0..PK_HUE_H {
            for x in 0..w {
                let hue = 360.0 * x as f32 / (w - 1).max(1) as f32;
                *self.grad_bits.add(((PK_SV_H + y) * w + x) as usize) =
                    dib_px(hsv_rgb(hue, 1.0, 1.0));
            }
        }
        GdiFlush();
        self.grad_hue = h;
    }

    unsafe fn draw_picker(&mut self, dc: HDC) {
        let Some(tool) = self.tool else { return };
        let color = self.styles[tool.idx()].color;
        let Some(h) = self.picker.as_ref().map(|p| p.h) else {
            return;
        };
        self.ensure_gradient(h);

        let Some(pk) = self.picker.as_ref() else { return };
        let (cw, ch) = (PK_SV_W, PK_SV_H);

        // 卡片。投影省了：面板下方常常是没有蒙层的截图，靠描边就够分清层次
        round_fill(dc, &pk.rect, ui(10), 0x00FF_FFFF);
        round_stroke(dc, &pk.rect, ui(10), rgb(0xD6, 0xD6, 0xDC));

        BitBlt(dc, pk.sv.left, pk.sv.top, cw, ch, self.grad_dc, 0, 0, SRCCOPY);
        crate::gdiplus::rect_stroke(dc, &pk.sv, 0x00D8D8DE, 1.0);
        BitBlt(
            dc,
            pk.hue.left,
            pk.hue.top,
            cw,
            PK_HUE_H,
            self.grad_dc,
            0,
            ch,
            SRCCOPY,
        );
        crate::gdiplus::rect_stroke(dc, &pk.hue, 0x00D8D8DE, 1.0);

        // SV 上的取色环：外白内黑，任何底色上都看得见
        let sx = pk.sv.left + (pk.s * (cw - 1) as f32) as i32;
        let sy = pk.sv.top + ((1.0 - pk.v) * (ch - 1) as f32) as i32;
        let outer = rect(sx - ui(5), sy - ui(5), sx + ui(5), sy + ui(5));
        crate::gdiplus::ellipse(dc, &outer, 0x00FF_FFFF, 2.4);
        let inner = rect(sx - ui(4), sy - ui(4), sx + ui(4), sy + ui(4));
        crate::gdiplus::ellipse(dc, &inner, rgb(0x20, 0x20, 0x28), 1.0);

        // 色相指针
        let hx = pk.hue.left + (pk.h * (cw - 1) as f32 / 360.0) as i32;
        let ptr_r = rect(hx - ui(2), pk.hue.top - ui(2), hx + ui(2), pk.hue.bottom + ui(2));
        round_fill(dc, &ptr_r, 4, 0x00FF_FFFF);
        crate::gdiplus::stroke_round(dc, &ptr_r, 2.0, rgb(0x1A, 0x1A, 0x20), 1.4);

        // Hex 输入框底色。字由 EDIT 自己画，擦背景时从 back buffer 拷，
        // 所以这里画的圆角能原样透出来。
        round_fill(dc, &pk.field, ui(6), rgb(0xF1, 0xF1, 0xF5));
        round_stroke(dc, &pk.field, ui(6), rgb(0xDD, 0xDD, 0xE3));

        // 吸管按钮
        let db = pk.dropper;
        let bg = if pk.hover_drop { rgb(0xE7, 0xE7, 0xEE) } else { rgb(0xF1, 0xF1, 0xF5) };
        round_fill(dc, &db, ui(6), bg);
        round_stroke(dc, &db, ui(6), rgb(0xDD, 0xDD, 0xE3));
        let (dcx, dcy) = ((db.left + db.right) / 2, (db.top + db.bottom) / 2);
        draw_pipette_icon(dc, dcx, dcy, ui(16), rgb(0x1A, 0x1A, 0x20));

        // 两个小标题
        let label = crate::gdiplus::FontSpec::new(ui(10) as f32, false, "Microsoft YaHei");
        crate::gdiplus::draw_text(
            dc,
            &label,
            rgb(0x9A, 0x9A, 0xA4),
            pk.sv.left,
            pk.field.bottom + PK_GAP,
            "最近",
        );

        // 最近使用：空位留淡底，位置固定，学得快
        for (i, r) in pk.recent_rects.iter().enumerate() {
            match self.recent.get(i) {
                Some(c) => chip_cell(dc, r, *c, *c == color, pk.hover == i as i32),
                None => round_fill(dc, r, ui(5), rgb(0xF4, 0xF4, 0xF7)),
            }
        }
        if let Some((first, _)) = pk.presets.first() {
            crate::gdiplus::draw_text(
                dc,
                &label,
                rgb(0x9A, 0x9A, 0xA4),
                pk.sv.left,
                first.top - PK_LABEL_H,
                "预设",
            );
        }
        for (i, (r, c)) in pk.presets.iter().enumerate() {
            chip_cell(
                dc,
                r,
                *c,
                *c == color,
                pk.hover == (RECENT_MAX + i) as i32,
            );
        }
    }


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
                        POINT {
                            x: self.cursor.x - self.screen.x,
                            y: self.cursor.y - self.screen.y,
                        },
                    );
                }
            }

            // 5.5 取色面板：盖在属性条上，也盖住选区内容
            if self.picker.is_some() {
                let dc = self.back_dc;
                self.draw_picker(dc);
            }

            // 6. 预备状态：光标下的像素颜色
            if !self.locked || self.eyedrop {
                draw_color_hud(
                    self.back_dc,
                    &self.screen,
                    self.cursor,
                    self.hex_mode,
                    self.label_font,
                    self.lens_dc,
                    self.lens_bits,
                );
            }

            // 6.5 录制倒计时，盖在最上层
            if self.countdown > 0 {
                if let Some(s) = self.selection() {
                    draw_countdown(self.back_dc, &s, self.countdown, self.count_font, self.label_font);
                }
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
            delete_font(self.count_font);
            if !self.grad_dc.is_null() {
                DeleteDC(self.grad_dc);
            }
            if !self.grad_bmp.is_null() {
                DeleteObject(self.grad_bmp as HGDIOBJ);
            }
            self.destroy_hex_edit();
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

unsafe fn draw_countdown(dc: HDC, sel: &RECT, n: i32, big: HFONT, small: HFONT) {
    let cx = (sel.left + sel.right) / 2;
    let cy = (sel.top + sel.bottom) / 2;

    let num = format!("{n}");
    let wide = crate::win::to_wide(&num);
    let sub = crate::win::to_wide("秒后开始录制");

    let old = SelectObject(dc, big);
    let mut sz: SIZE = std::mem::zeroed();
    GetTextExtentPoint32W(dc, wide.as_ptr(), (wide.len() - 1) as i32, &mut sz);

    SelectObject(dc, small);
    let mut sz2: SIZE = std::mem::zeroed();
    GetTextExtentPoint32W(dc, sub.as_ptr(), (sub.len() - 1) as i32, &mut sz2);
    SelectObject(dc, old);

    let pad = ui(26);
    let gap = ui(6);
    let panel_w = sz.cx.max(sz2.cx) + pad * 2;
    let panel_h = sz.cy + gap + sz2.cy + pad * 2;
    let panel = rect(cx - panel_w / 2, cy - panel_h / 2, cx + panel_w / 2, cy + panel_h / 2);
    round_fill(dc, &panel, ui(18), rgb(0x00, 0x00, 0x00));
    round_stroke(dc, &panel, ui(18), rgb(0x3A, 0x3A, 0x44));

    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(dc, rgb(0xFF, 0xFF, 0xFF));
    SelectObject(dc, big);
    TextOutW(
        dc,
        cx - sz.cx / 2,
        panel.top + pad,
        wide.as_ptr(),
        (wide.len() - 1) as i32,
    );

    SetTextColor(dc, rgb(0xB4, 0xB4, 0xBE));
    SelectObject(dc, small);
    TextOutW(
        dc,
        cx - sz2.cx / 2,
        panel.top + pad + sz.cy + gap,
        sub.as_ptr(),
        (sub.len() - 1) as i32,
    );
    SelectObject(dc, old);
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

        if b.act == Act::Record {
            // 雪碧图里没有录制图标，直接画一个实心红点
            let cx = (b.rect.left + b.rect.right) / 2;
            let cy = (b.rect.top + b.rect.bottom) / 2;
            let r = ui(8);
            crate::gdiplus::fill_round(
                dc,
                &rect(cx - r, cy - r, cx + r, cy + r),
                r as f32,
                rgb(0xFF, 0x2E, 0x63),
            );
        } else {
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
    }

    round_stroke(dc, &bar, RADIUS_BAR, rgb(0xD6, 0xD6, 0xDC));
}

/// 面板里的一个色格：圆角色块 + 描边。选中时描边加粗转深，
/// 悬停时也加深一档——白底上的白色块没有描边就看不见了。
unsafe fn chip_cell(dc: HDC, r: &RECT, color: u32, selected: bool, hover: bool) {
    round_fill(dc, r, ui(5), color);
    let (line, w) = if selected {
        (rgb(0x1A, 0x1A, 0x20), 2.0)
    } else if hover {
        (rgb(0x6E, 0x6E, 0x78), 1.4)
    } else {
        (rgb(0xDD, 0xDD, 0xE3), 1.0)
    };
    crate::gdiplus::stroke_round(dc, r, ui(5) as f32, line, w);
}

/// 调色板图标：正圆在右下缺一角当拇指位，里面五个颜料点。
///
/// 轮廓是两段真圆弧（外圆 + 缺口圆）首尾相接，交点用两圆相交公式算。
/// 不用采样折线：半径只有十几像素，采样点取整后轮廓会出现平段。
unsafe fn draw_palette_icon(dc: HDC, cx: i32, cy: i32, size: i32, color: u32) {
    let s = size as f32;
    let (fx, fy) = (cx as f32, cy as f32);
    let r = s * 0.42;
    // 缺口圆：圆心在外圆外侧的右下，半径小，只咬掉一个角
    let (nx, ny) = (fx + r * 1.02 * 0.72, fy + r * 1.02 * 0.70);
    let nr = r * 0.26;

    let to_deg = |p: (f32, f32), c: (f32, f32)| (p.1 - c.1).atan2(p.0 - c.0).to_degrees();
    let norm = |v: f32| {
        let m = v % 360.0;
        if m < 0.0 {
            m + 360.0
        } else {
            m
        }
    };

    // 两圆交点
    let (ddx, ddy) = (nx - fx, ny - fy);
    let d = (ddx * ddx + ddy * ddy).sqrt().max(0.001);
    let mid = (d * d + r * r - nr * nr) / (2.0 * d);
    let half = (r * r - mid * mid).max(0.0).sqrt();
    let (mx, my) = (fx + ddx * mid / d, fy + ddy * mid / d);
    let (px, py) = (-ddy / d, ddx / d);
    let p1 = (mx + px * half, my + py * half);
    let p2 = (mx - px * half, my - py * half);

    let a1 = to_deg(p1, (fx, fy));
    let a2 = to_deg(p2, (fx, fy));
    // 保留的是"绕远路"那一段：缺口只占很小一个角
    let mut sweep = norm(a2 - a1);
    if sweep <= 180.0 {
        sweep -= 360.0;
    }
    let n1 = to_deg(p2, (nx, ny));
    // 缺口那段要"朝里鼓"：两个方向的弧各试中点，取离外圆圆心更近的那个
    let n2 = to_deg(p1, (nx, ny));
    let mut nsweep = norm(n2 - n1);
    if nsweep > 180.0 {
        nsweep -= 360.0;
    }
    let bulge = |sw: f32| {
        let mid = (n1 + sw * 0.5).to_radians();
        let (mx, my) = (nx + nr * mid.cos(), ny + nr * mid.sin());
        (mx - fx) * (mx - fx) + (my - fy) * (my - fy)
    };
    if bulge(nsweep) > bulge(nsweep + 360.0) {
        nsweep += 360.0;
    }

    let arcs = [
        crate::gdiplus::Arc {
            x: fx - r,
            y: fy - r,
            w: r * 2.0,
            h: r * 2.0,
            start: a1,
            sweep,
        },
        crate::gdiplus::Arc {
            x: nx - nr,
            y: ny - nr,
            w: nr * 2.0,
            h: nr * 2.0,
            start: n1,
            sweep: nsweep,
        },
    ];
    crate::gdiplus::stroke_arcs(dc, &arcs, true, color, (s * 0.045).max(1.2));

    // 五个点沿上半圈弧线排开，从左中走到右上
    let dot = (s * 0.052).max(1.3);
    for k in 0..5 {
        let a = (184.0 + k as f32 * 38.0).to_radians();
        let dx = fx + r * 0.48 * a.cos();
        let dy = fy + r * 0.48 * a.sin();
        let d = rect(
            (dx - dot).round() as i32,
            (dy - dot).round() as i32,
            (dx + dot).round() as i32,
            (dy + dot).round() as i32,
        );
        crate::gdiplus::fill_ellipse(dc, &d, color);
    }
}

/// 吸管图标：沿 45° 斜放，左下一截笔尖、中间一道卡箍、右上一个圆头。
///
/// `size` 是图标的外接方框边长。轴向长度按对角线算（1.32 倍），
/// 这样斜着放也能把方框填满，不会显得比旁边图标小一圈。
unsafe fn draw_pipette_icon(dc: HDC, cx: i32, cy: i32, size: i32, color: u32) {
    let s = size as f32;
    let len = s * 1.32;
    // u 是轴向（指向右上），v 与它垂直
    let a = (-45.0f32).to_radians();
    let (ux, uy) = (a.cos(), a.sin());
    let (vx, vy) = (-uy, ux);
    let (ox, oy) = (cx as f32 - ux * len * 0.5, cy as f32 - uy * len * 0.5);

    let quad = |t0: f32, w0: f32, t1: f32, w1: f32| -> Vec<POINT> {
        let p = |t: f32, w: f32| POINT {
            x: (ox + ux * t + vx * w).round() as i32,
            y: (oy + uy * t + vy * w).round() as i32,
        };
        vec![p(t0, w0), p(t1, w1), p(t1, -w1), p(t0, -w0)]
    };
    let dot = |t: f32, r: f32| RECT {
        left: (ox + ux * t - r).round() as i32,
        top: (oy + uy * t - r).round() as i32,
        right: (ox + ux * t + r).round() as i32,
        bottom: (oy + uy * t + r).round() as i32,
    };

    // 笔尖（圆头）+ 笔杆
    crate::gdiplus::fill_ellipse(dc, &dot(len * 0.005, len * 0.055), color);
    crate::gdiplus::fill_polygon(dc, &quad(len * 0.005, len * 0.055, len * 0.48, len * 0.07), color);
    // 卡箍：横跨笔杆的一道薄条
    crate::gdiplus::fill_polygon(dc, &quad(len * 0.48, len * 0.165, len * 0.55, len * 0.165), color);
    // 圆头是沿轴向稍稍拉长的水滴：两个圆叠成胶囊，比正圆更像滴管
    crate::gdiplus::fill_ellipse(dc, &dot(len * 0.775, len * 0.115), color);
    crate::gdiplus::fill_ellipse(dc, &dot(len * 0.845, len * 0.115), color);
}

unsafe fn draw_props(
    dc: HDC,
    props: &[(RECT, Prop)],
    hover_prop: i32,
    style: &Style,
    tool: Tool,
    cursor: POINT,
) {
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
            // 当前色不在预设里，就把这一格点亮，否则界面上看不出选了啥
            Prop::Custom => !COLORS.contains(&style.color),
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
            Prop::Custom => {
                // 调色板：点开就是取色面板。单色描边，和工具条上其它图标一致
                draw_palette_icon(dc, cx, cy, ui(20), rgb(0x1A, 0x1A, 0x20));
            }
            Prop::Width => {
                // 半径体现粗细。按各自的取值区间归一化，滑块感才准：
                // 形状 1~24、橡皮 6~80、文本 10~72，同一个点位置含义一致。
                let (lo, hi) = tool.width_range();
                let k = (style.width.clamp(lo, hi) - lo) as f32 / (hi - lo).max(1) as f32;
                let r = 3.5 + k * 8.0;
                let d = rect(cx - r as i32, cy - r as i32, cx + r as i32, cy + r as i32);
                crate::gdiplus::fill_ellipse(dc, &d, rgb(ON.0, ON.1, ON.2));
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

    // 光标停在粗细格上时，上方浮一个当前数值
    if hover_prop >= 0 {
        if let Some((_, Prop::Width)) = props.get(hover_prop as usize) {
            let text = format!("{} px", style.width);
            let wide = to_wide(&text);
            let font = crate::win::ui_font_w(ui(11), 600);
            let old = SelectObject(dc, font as HGDIOBJ);
            let mut sz: windows_sys::Win32::Foundation::SIZE = std::mem::zeroed();
            GetTextExtentPoint32W(dc, wide.as_ptr(), (wide.len() - 1) as i32, &mut sz);
            let (pad_x, pad_y) = (ui(7), ui(4));
            let (bw, bh) = (sz.cx + pad_x * 2, sz.cy + pad_y * 2);
            // 跟着光标走，靠边就翻到另一侧
            let mut bx = cursor.x + ui(10);
            if bx + bw > bar.right {
                bx = cursor.x - bw - ui(10);
            }
            let mut by = cursor.y + ui(18);
            if by + bh > bar.bottom + ui(120) {
                by = cursor.y - bh - ui(12);
            }
            round_fill(
                dc,
                &rect(bx, by, bx + bw, by + bh),
                ui(6),
                rgb(0x2E, 0x2E, 0x36),
            );
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, rgb(0xFF, 0xFF, 0xFF));
            TextOutW(
                dc,
                bx + pad_x,
                by + pad_y,
                wide.as_ptr(),
                (wide.len() - 1) as i32,
            );
            SelectObject(dc, old);
            DeleteObject(font as HGDIOBJ);
        }
    }
}

/// 预备截图的取色浮层：色块 + 光标下像素的颜色值。
///
/// 跟着光标走，右下偏移；贴到屏幕边就翻到另一侧，始终完整可见。
unsafe fn draw_color_hud(
    dc: HDC,
    screen: &Screen,
    cursor: POINT,
    hex: bool,
    font: HFONT,
    lens_dc: HDC,
    lens_bits: *mut u32,
) {
    let Some((r, g, b)) = sample_pixel(screen, cursor.x, cursor.y) else {
        return;
    };

    // 放大 LENS_N×LENS_N：中心那一格就是光标所在像素
    let n = LENS_N;
    let zoom = ui(4).max(5);
    let grid = n * zoom;
    for dy in 0..n {
        for dx in 0..n {
            let (pr, pg, pb) = sample_pixel(screen, cursor.x + dx - n / 2, cursor.y + dy - n / 2)
                .unwrap_or((0, 0, 0));
            let px = ((pr as u32) << 16) | ((pg as u32) << 8) | pb as u32;
            *lens_bits.add((dy * n + dx) as usize) = px;
        }
    }

    let text = color_text(r, g, b, hex);
    let coord = format!("{}, {}", cursor.x, cursor.y);
    let small = ui_font_w(ui(10), 400);

    let measure = |f: HFONT, s: &str| -> (i32, i32) {
        let wide = to_wide(s);
        let old = SelectObject(dc, f as HGDIOBJ);
        let mut sz: windows_sys::Win32::Foundation::SIZE = std::mem::zeroed();
        GetTextExtentPoint32W(dc, wide.as_ptr(), (wide.len() - 1) as i32, &mut sz);
        SelectObject(dc, old);
        (sz.cx, sz.cy)
    };
    let (w1, h1) = measure(font, &text);
    let (w2, h2) = measure(small, &coord);

    let pad = ui(8);
    let gap = ui(6);
    let sw = ui(22);
    let text_w = w1.max(w2);
    let text_h = h1 + ui(2) + h2;
    let content_w = grid.max(sw + gap + text_w);
    let row_h = sw.max(text_h);
    let bw = pad * 2 + content_w;
    let bh = pad * 2 + grid + gap + row_h;

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

    // 放大镜本体：COLORONCOLOR 就是最近邻，放大后每格都是纯色方块
    let gx = bx + pad + (content_w - grid) / 2;
    let gy = by + pad;
    SetStretchBltMode(dc, COLORONCOLOR);
    StretchBlt(dc, gx, gy, grid, grid, lens_dc, 0, 0, n, n, SRCCOPY);
    frame_colored(
        dc,
        &rect(gx - 1, gy - 1, gx + grid + 1, gy + grid + 1),
        rgb(0x4A, 0x4A, 0x52),
    );

    // 中心格：外黑内白两道框，深浅底上都看得清
    let ccx = gx + (n / 2) * zoom;
    let ccy = gy + (n / 2) * zoom;
    frame(dc, &rect(ccx, ccy, ccx + zoom, ccy + zoom));

    // 下面一行：色块 + 颜色值 + 坐标
    let row_y = gy + grid + gap;
    let chip = rect(
        bx + pad,
        row_y + (row_h - sw) / 2,
        bx + pad + sw,
        row_y + (row_h + sw) / 2,
    );
    round_fill(dc, &chip, ui(4), rgb(r, g, b));
    round_stroke(dc, &chip, ui(4), rgb(0x6A, 0x6A, 0x72));

    let tx = bx + pad + sw + gap;
    let ty = row_y + (row_h - text_h) / 2;
    SetBkMode(dc, TRANSPARENT as i32);

    SetTextColor(dc, rgb(0xFF, 0xFF, 0xFF));
    let old_f = SelectObject(dc, font as HGDIOBJ);
    let wide = to_wide(&text);
    TextOutW(dc, tx, ty, wide.as_ptr(), (wide.len() - 1) as i32);
    SelectObject(dc, old_f);

    SetTextColor(dc, rgb(0x9A, 0x9A, 0xA4));
    let old_s = SelectObject(dc, small as HGDIOBJ);
    let wide2 = to_wide(&coord);
    TextOutW(
        dc,
        tx,
        ty + h1 + ui(2),
        wide2.as_ptr(),
        (wide2.len() - 1) as i32,
    );
    SelectObject(dc, old_s);
    DeleteObject(small as HGDIOBJ);
}

/// 1px 描边矩形（GDI 的描边是精确像素，比 GDI+ 的半像素更锐）
unsafe fn frame_colored(dc: HDC, r: &RECT, color: u32) {
    let pen = CreatePen(PS_SOLID, 1, color);
    let old_p = SelectObject(dc, pen as HGDIOBJ);
    let old_b = SelectObject(dc, GetStockObject(HOLLOW_BRUSH) as HGDIOBJ);
    Rectangle(dc, r.left, r.top, r.right, r.bottom);
    SelectObject(dc, old_p);
    SelectObject(dc, old_b);
    DeleteObject(pen as HGDIOBJ);
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

                // 取色面板独占鼠标：拖动取色，其余逻辑全部让路
                if ov.picker.is_some() {
                    let drag = ov.picker.as_ref().map(|p| p.drag).unwrap_or(0);
                    if drag != 0 {
                        ov.picker_drag(x, y);
                    } else if ov.picker_hover(x, y) {
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                    return 0;
                }

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

                // 光标位置：取色浮层和属性条的提示框都要用
                let (sx, sy) = (x + ov.screen.x, y + ov.screen.y);
                if ov.cursor.x != sx || ov.cursor.y != sy {
                    ov.cursor = POINT { x: sx, y: sy };
                    InvalidateRect(hwnd, std::ptr::null(), 0);
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
            // 滚轮：光标停在粗细格上时调整线宽
            WM_MOUSEWHEEL => {
                let delta = ((wparam >> 16) & 0xFFFF) as i16 as i32;
                if delta != 0 && ov.picker.is_none() {
                    if let Some(tool) = ov.tool {
                        let on_width = matches!(
                            ov.props.get(ov.hover_prop.max(0) as usize),
                            Some((_, Prop::Width))
                        );
                        if on_width {
                            let (lo, hi) = tool.width_range();
                            let st = &mut ov.styles[tool.idx()];
                            let step = if delta > 0 { 1 } else { -1 };
                            st.width = (st.width + step).clamp(lo, hi);
                            save_styles(ov);
                            InvalidateRect(hwnd, std::ptr::null(), 0);
                        }
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
                // 吸管模式：点哪取哪，取完就退出
                if ov.eyedrop {
                    if let Some((r, g, b)) = sample_pixel(&ov.screen, x + ov.screen.x, y + ov.screen.y)
                    {
                        ov.set_color(cref(r, g, b), true);
                    }
                    ov.eyedrop = false;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    return 0;
                }
                // 面板开着时只处理面板
                if ov.picker.is_some() {
                    ov.picker_click(x, y);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    return 0;
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
                // 面板里的拖拽收尾：抬手时才记一次最近使用并写配置
                if ov.picker.is_some() {
                    let drag = ov.picker.as_ref().map(|p| p.drag).unwrap_or(0);
                    if drag != 0 {
                        if let Some(p) = ov.picker.as_mut() {
                            p.drag = 0;
                        }
                        ReleaseCapture();
                        if let Some(tool) = ov.tool {
                            let c = ov.styles[tool.idx()].color;
                            ov.push_recent(c);
                            save_styles(ov);
                        }
                        InvalidateRect(hwnd, std::ptr::null(), 0);
                    }
                    return 0;
                }
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
                                // 面板里的取色目标跟着工具走，换工具就收起面板
                                ov.close_picker();
                                ov.tool = if ov.tool == Some(t) { None } else { Some(t) };
                                crate::win::trace(&format!("tool -> {:?}", ov.tool));
                                ov.layout_buttons();
                                InvalidateRect(hwnd, std::ptr::null(), 0);
                            }
                            Act::Copy => ov.finish(CMD_COPY),
                            Act::Save => ov.finish(CMD_SAVE),
                            Act::Pin => ov.finish(CMD_PIN),
                            Act::Record => {
                                let secs = crate::config::Config::load().record_countdown;
                                if secs == 0 {
                                    ov.finish(CMD_RECORD);
                                } else {
                                    ov.countdown = secs as i32;
                                    SetTimer(hwnd, TIMER_COUNTDOWN, 1000, None);
                                    InvalidateRect(hwnd, std::ptr::null(), 0);
                                }
                            }
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

                // 面板开着：Esc / 回车都只是收起面板，不往下面的分支走
                if ov.picker.is_some() && (vk == VK_ESCAPE as i32 || vk == VK_RETURN as i32) {
                    ov.close_picker();
                    return 0;
                }
                if ov.eyedrop && vk == VK_ESCAPE as i32 {
                    ov.eyedrop = false;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    return 0;
                }

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
            WM_HEX_COMMIT => {
                ov.commit_hex();
                0
            }
            WM_HEX_CANCEL => {
                ov.sync_hex_text();
                SetFocus(hwnd);
                0
            }
            // 输入框的配色：字色跟着当前设置走，底色反着来
            // 输入框的背景由子类的 WM_ERASEBKGND 从 back buffer 拷过来，
            // 这里只把字色给它、背景刷成空心，看起来就是"透明"的
            WM_CTLCOLOREDIT => {
                let hdc = wparam as HDC;
                // Hex 框的字不能跟着当前颜色走：选了白色就白字白底，看不见了
                if !ov.hex_edit.is_null() && lparam as HWND == ov.hex_edit {
                    SetTextColor(hdc, rgb(0x1A, 0x1A, 0x20));
                } else {
                    let st = ov.styles[Tool::Text.idx()];
                    SetTextColor(hdc, st.color);
                }
                SetBkMode(hdc, TRANSPARENT as i32);
                GetStockObject(HOLLOW_BRUSH) as LRESULT
            }
            // 换行后把输入框撑高
            WM_COMMAND => {
                let code = ((wparam >> 16) & 0xFFFF) as u32;
                let target = lparam as HWND;
                if code == EN_CHANGE {
                    if !ov.text_edit.is_null() && target == ov.text_edit {
                        ov.fit_text_box();
                    } else if !ov.hex_edit.is_null() && target == ov.hex_edit && !ov.hex_sync {
                        ov.hex_live();
                    }
                }
                0
            }
            // 锁定后：边上显示缩放光标，内部显示移动光标
            WM_SETCURSOR => {
                // 面板上要箭头，吸管要十字（类光标本来就是自绘十字）
                if ov.picker.is_some() {
                    SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_ARROW));
                    return 1;
                }
                if ov.eyedrop {
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                }
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
            WM_TIMER if wparam == TIMER_COUNTDOWN => {
                ov.countdown -= 1;
                if ov.countdown <= 0 {
                    KillTimer(hwnd, TIMER_COUNTDOWN);
                    ov.finish(CMD_RECORD);
                } else {
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                0
            }
            WM_PAINT => {
                ov.paint();
                0
            }
            windows_sys::Win32::UI::WindowsAndMessaging::WM_DESTROY => {
                ov.destroy_edit();
                ov.destroy_hex_edit();
                OVERLAY = None;
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

unsafe fn apply_prop(ov: &mut Overlay, idx: i32) {
    let Some(tool) = ov.tool else { return };
    let prop = match ov.props.get(idx as usize) {
        Some((_, p)) => *p,
        None => return,
    };
    crate::win::trace(&format!("prop {prop:?} for {tool:?}"));

    // 打开面板不动样式，也就没有要存的；其余分支统一收尾保存
    if prop == Prop::Custom {
        ov.open_picker();
        return;
    }

    let st = &mut ov.styles[tool.idx()];
    match prop {
        Prop::Color(i) => st.color = COLORS[i.min(COLORS.len() - 1)],
        Prop::Custom | Prop::Width => {}
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
///
/// 最近使用色也搭这趟车一起落盘：它在内存里是独立的一份，
/// 不跟过来的话每次重启就清空了。
fn save_styles(ov: &Overlay) {
    let mut cfg = crate::config::Config::load();
    for i in 0..5 {
        cfg.styles[i] = Some(ov.styles[i].to_token());
    }
    cfg.recent_colors = ov.recent.clone();
    if !cfg.save() {
        crate::win::trace("config: 样式保存失败");
    }
}

/// 十字光标每一段线段的长度（像素）。取偶数，图案才能对中心镜像对称。
const CROSS_SEG: i32 = 4;

/// 自己画一个"黑白交替"的十字光标，替掉系统的 `IDC_CROSS`。
///
/// 系统那个十字是纯黑单色，压在深色桌面上就消失了。这里不选"反色"方案：
/// 反色在 50% 中性灰上恰好失效（128 反色是 127），而那是很常见的背景。
/// 黑白交替则是相邻两段颜色互为正反面，任何底色上总有一半线段对比度够高，
/// 眼睛靠这几段就能把整条十字连起来。
///
/// 位图按中心镜像对称，所以 `CreateBitmap` 要求的 bottom-up 行序对图案没有影响
/// （上下翻转后像素完全一样），省掉一个容易写错的地方。
unsafe fn make_cross_cursor() -> HCURSOR {
    let n = GetSystemMetrics(SM_CXCURSOR).max(16);
    let c = n / 2;
    // 1bpp 行按 WORD 对齐；位在字节里是从高位往低位排
    let stride = (((n + 15) / 16) * 2) as usize;
    let mut color = vec![0u32; (n * n) as usize];
    // 单色遮罩里 1 = 透明。即使系统不认 32bpp 的 alpha，形状也能靠它保住。
    let mut mask = vec![0u8; stride * n as usize];

    for y in 0..n {
        for x in 0..n {
            if x != c && y != c {
                let byte = y as usize * stride + (x / 8) as usize;
                mask[byte] |= 0x80 >> (x % 8);
                continue;
            }
            // 黑白沿两条轴各自交替，距中心 SE段 换一次色
            let d = (if x == c { y - c } else { x - c }).abs();
            let white = (d / CROSS_SEG) % 2 == 1;
            // 0xAARRGGBB；纯黑纯白在 R/B 颠倒时也不变，不用管通道顺序
            color[(y * n + x) as usize] = if white { 0xFFFF_FFFF } else { 0xFF00_0000 };
        }
    }

    let hbm_color = CreateBitmap(n, n, 1, 32, color.as_ptr() as *const core::ffi::c_void);
    let hbm_mask = CreateBitmap(n, n, 1, 1, mask.as_ptr() as *const core::ffi::c_void);
    if hbm_color.is_null() || hbm_mask.is_null() {
        if !hbm_color.is_null() {
            DeleteObject(hbm_color as HGDIOBJ);
        }
        if !hbm_mask.is_null() {
            DeleteObject(hbm_mask as HGDIOBJ);
        }
        return LoadCursorW(std::ptr::null_mut(), IDC_CROSS);
    }

    let info = ICONINFO {
        fIcon: 0, // FALSE = 光标
        xHotspot: c as u32,
        yHotspot: c as u32,
        hbmMask: hbm_mask,
        hbmColor: hbm_color,
    };
    let cur = CreateIconIndirect(&info);
    // 位图已经交给光标对象，原始位图不再需要
    DeleteObject(hbm_color as HGDIOBJ);
    DeleteObject(hbm_mask as HGDIOBJ);

    if cur.is_null() {
        crate::win::trace("overlay: 十字光标创建失败，回退系统光标");
        return LoadCursorW(std::ptr::null_mut(), IDC_CROSS);
    }
    cur
}

pub fn register() {
    unsafe {
        let class = to_wide(CLASS_NAME);
        let wc = WNDCLASSW {
            style: 0x0002 | 0x0001, // CS_HREDRAW | CS_VREDRAW
            lpfnWndProc: Some(overlay_wndproc),
            hInstance: hinstance(),
            // 十字光标：黑白交替，任何底色上都看得见
            hCursor: make_cross_cursor(),
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

        let lens_dc = CreateCompatibleDC(desktop_dc);
        let mut lens_bi: BITMAPINFO = std::mem::zeroed();
        lens_bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        lens_bi.bmiHeader.biWidth = LENS_N;
        lens_bi.bmiHeader.biHeight = -LENS_N; // 负高度 = 自上而下，行序更直观
        lens_bi.bmiHeader.biPlanes = 1;
        lens_bi.bmiHeader.biBitCount = 32;
        lens_bi.bmiHeader.biCompression = BI_RGB;
        let mut lens_bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let lens_bmp = CreateDIBSection(
            desktop_dc,
            &lens_bi,
            DIB_RGB_COLORS,
            &mut lens_bits,
            std::ptr::null_mut(),
            0,
        );
        SelectObject(lens_dc, lens_bmp as HGDIOBJ);

        // 取色面板的 HSV 渐变缓存：SV 方块在上、色相条在下，共用一张 DIB
        let grad_dc = CreateCompatibleDC(desktop_dc);
        let mut grad_bi: BITMAPINFO = std::mem::zeroed();
        grad_bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        grad_bi.bmiHeader.biWidth = PK_SV_W;
        grad_bi.bmiHeader.biHeight = -(PK_SV_H + PK_HUE_H);
        grad_bi.bmiHeader.biPlanes = 1;
        grad_bi.bmiHeader.biBitCount = 32;
        grad_bi.bmiHeader.biCompression = BI_RGB;
        let mut grad_bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let grad_bmp = CreateDIBSection(
            desktop_dc,
            &grad_bi,
            DIB_RGB_COLORS,
            &mut grad_bits,
            std::ptr::null_mut(),
            0,
        );
        SelectObject(grad_dc, grad_bmp as HGDIOBJ);

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
            lens_dc,
            lens_bmp,
            lens_bits: lens_bits as *mut u32,
            mask_dc,
            mask_bmp,
            label_font: ui_font(ui(12)),
            countdown: 0,
            count_font: ui_font(ui(64)),
            picker: None,
            hex_edit: std::ptr::null_mut(),
            hex_font: std::ptr::null_mut(),
            hex_sync: false,
            grad_dc,
            grad_bmp,
            grad_bits: grad_bits as *mut u32,
            grad_hue: -1.0,
            recent: {
                let mut r = crate::config::Config::load().recent_colors;
                r.truncate(RECENT_MAX);
                r
            },
            eyedrop: false,
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
            ov.cursor = POINT { x: cp.x, y: cp.y };
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

/// 录制用的选区（屏幕物理像素）。和 take_pending 分开，避免为了拿坐标去裁剪整张图。
pub fn take_rect() -> RECT {
    unsafe { PENDING_RECT }
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
