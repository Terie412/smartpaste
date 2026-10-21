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
use crate::win::{delete_font, hinstance, pt_in, rect, to_wide, ui_font};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    AlphaBlend, BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreatePen,
    CreateSolidBrush, DeleteDC, DeleteObject, Ellipse, EndPaint, FillRect, GetDC, GetStockObject,
    GetTextExtentPoint32W, InvalidateRect, IntersectClipRect, Rectangle, ReleaseDC, RestoreDC,
    RoundRect, SaveDC, SelectObject, SetBkMode, SetTextColor, TextOutW, AC_SRC_OVER, BLENDFUNCTION,
    HDC, HFONT, HGDIOBJ, NULL_BRUSH, NULL_PEN, PAINTSTRUCT, PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_CONTROL, VK_ESCAPE, VK_RETURN, VK_S,
    VK_Z,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, PostMessageW, RegisterClassW,
    SetForegroundWindow, ShowWindow, SW_SHOW, WM_APP, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_PAINT, WM_RBUTTONUP, WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    WS_VISIBLE,
};

pub const CMD_NONE: u32 = 0;
pub const CMD_COPY: u32 = 1;
pub const CMD_SAVE: u32 = 2;
pub const CMD_PIN: u32 = 3;
pub const CMD_CANCEL: u32 = 4;

/// 遮罩窗口结束后，把结果发回主窗口
pub const WM_SNAP_DONE: u32 = WM_APP + 2;

const CLASS_NAME: &str = "SmartPasteOverlay";
const MASK_ALPHA: u8 = 128;

// RoundRect 的后两个参数是圆角椭圆的宽高，也就是半径的两倍
const RADIUS_BAR: i32 = 16;
const RADIUS_CHIP: i32 = 12;

const BTN_W: i32 = 48;
const BTN_H: i32 = 34;
const GAP: i32 = 8;

/// 属性条上一格的大小
const PROP_W: i32 = 30;
const PROP_H: i32 = 34;

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
    Eraser,
}

impl Tool {
    fn idx(self) -> usize {
        match self {
            Tool::Shape => 0,
            Tool::Arrow => 1,
            Tool::Pen => 2,
            Tool::Eraser => 3,
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

        static SHAPE: [Prop; 12] = [
            Prop::Round(false),
            Prop::Round(true),
            Prop::Color(0),
            Prop::Color(1),
            Prop::Color(2),
            Prop::Color(3),
            Prop::Color(4),
            Prop::Color(5),
            Prop::Filled,
            Prop::Width(0),
            Prop::Width(1),
            Prop::Width(2),
        ];
        static ARROW: [Prop; 10] = [
            Prop::Color(0),
            Prop::Color(1),
            Prop::Color(2),
            Prop::Color(3),
            Prop::Color(4),
            Prop::Color(5),
            Prop::Dashed,
            Prop::Width(0),
            Prop::Width(1),
            Prop::Width(2),
        ];
        static PEN: [Prop; 9] = [
            Prop::Color(0),
            Prop::Color(1),
            Prop::Color(2),
            Prop::Color(3),
            Prop::Color(4),
            Prop::Color(5),
            Prop::Width(0),
            Prop::Width(1),
            Prop::Width(2),
        ];
        static ERASER: [Prop; 5] = [
            Prop::Width(0),
            Prop::Width(1),
            Prop::Width(2),
            Prop::Round(false),
            Prop::Round(true),
        ];

        let _ = (COLORS_6, WIDTHS_3);
        match self {
            Tool::Shape => &SHAPE,
            Tool::Arrow => &ARROW,
            Tool::Pen => &PEN,
            Tool::Eraser => &ERASER,
        }
    }

    /// 笔触档位因工具而异：橡皮明显要粗得多
    fn width_at(self, i: usize) -> i32 {
        match self {
            Tool::Eraser => [12, 24, 40][i.min(2)],
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
    /// 四个工具各自一套设置，切换工具不会串味
    pub styles: [Style; 4],
    pub draft: Option<Shape>,
    pub drawing: bool,
    pub doc: Doc,
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
            (Act::Tool(Tool::Eraser), 8),
            (Act::Copy, 0),
            (Act::Save, 1),
            (Act::Pin, 2),
        ];

        let total = items.len() as i32 * BTN_W;
        let mut x = sel.right - total;
        if x < 0 {
            x = 0;
        }
        let mut y = sel.bottom + GAP;
        if y + BTN_H + 4 > self.screen.height {
            y = sel.top - BTN_H - GAP;
        }
        if y < 4 {
            y = (sel.bottom + GAP).min(self.screen.height - BTN_H - 4);
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
        let pw = list.len() as i32 * PROP_W;
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
            self.props.push((rect(px, py, px + PROP_W, py + PROP_H), *prop));
            px += PROP_W;
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
            let Some(base) = self
                .screen
                .crop_rgba(sel.left, sel.top, sel.right - sel.left, sel.bottom - sel.top)
            else {
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

    fn stroke_begin(&mut self, x: i32, y: i32) {
        let Some(sel) = self.selection() else { return };
        let Some(tool) = self.tool else { return };
        crate::win::trace(&format!("stroke begin {tool:?} at {x},{y}"));
        let p = POINT {
            x: x - sel.left,
            y: y - sel.top,
        };
        let kind = match tool {
            Tool::Arrow => Kind::Arrow { a: p, b: p },
            Tool::Shape => Kind::Box { a: p, b: p },
            Tool::Pen => Kind::Pen { pts: vec![p] },
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

            // 2. 蒙层
            match self.selection() {
                Some(s) => {
                    blend(self.mask_dc, self.back_dc, 0, 0, w, s.top);
                    blend(self.mask_dc, self.back_dc, 0, s.top, s.left, s.bottom);
                    blend(self.mask_dc, self.back_dc, s.right, s.top, w, s.bottom);
                    blend(self.mask_dc, self.back_dc, 0, s.bottom, w, h);
                }
                None => blend(self.mask_dc, self.back_dc, 0, 0, w, h),
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

            // 6. 上屏
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

/// 圆角填充，无边框。
unsafe fn round_fill(dc: HDC, r: &RECT, dia: i32, color: u32) {
    let brush = CreateSolidBrush(color);
    let old_pen = SelectObject(dc, GetStockObject(NULL_PEN) as HGDIOBJ);
    let old_brush = SelectObject(dc, brush as HGDIOBJ);
    RoundRect(dc, r.left, r.top, r.right, r.bottom, dia, dia);
    SelectObject(dc, old_brush);
    SelectObject(dc, old_pen);
    DeleteObject(brush as HGDIOBJ);
}

/// 圆角描边，不填充。
unsafe fn round_stroke(dc: HDC, r: &RECT, dia: i32, color: u32) {
    round_stroke_w(dc, r, dia, color, 1);
}

/// 圆角描边，可以指定线宽。
unsafe fn round_stroke_w(dc: HDC, r: &RECT, dia: i32, color: u32, w: i32) {
    let pen = CreatePen(PS_SOLID, w, color);
    let old_pen = SelectObject(dc, pen as HGDIOBJ);
    let old_brush = SelectObject(dc, GetStockObject(NULL_BRUSH) as HGDIOBJ);
    RoundRect(dc, r.left, r.top, r.right, r.bottom, dia, dia);
    SelectObject(dc, old_brush);
    SelectObject(dc, old_pen);
    DeleteObject(pen as HGDIOBJ);
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

    let (pad_x, pad_y) = (8, 4);
    let bw = size.cx + pad_x * 2;
    let bh = size.cy + pad_y * 2;
    let bx = s.left;
    let by = if s.top >= bh + 6 {
        s.top - bh - 6
    } else {
        s.top + 6
    };

    let bg = rect(bx, by, bx + bw, by + bh);
    round_fill(hdc, &bg, RADIUS_CHIP, rgb(0x0A, 0x0A, 0x0C));

    SetBkMode(hdc, TRANSPARENT as i32);
    SetTextColor(hdc, rgb(0xFF, 0xFF, 0xFF));
    TextOutW(hdc, bx + pad_x, by + pad_y, wide.as_ptr(), (wide.len() - 1) as i32);
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
    let split = 4usize;
    if split < buttons.len() {
        let sep_brush = CreateSolidBrush(rgb(0xD0, 0xD0, 0xD6));
        let x = buttons[split].rect.left;
        let sep = rect(x, bar.top + 8, x + 1, bar.bottom - 8);
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
            // 选中：比悬停块大一圈，再加 2px 深色边框，
            // 光靠浅灰底在白工具栏上看不出来
            let r = rect(
                b.rect.left + 2,
                b.rect.top + 2,
                b.rect.right - 2,
                b.rect.bottom - 2,
            );
            round_fill(dc, &r, RADIUS_CHIP, rgb(0xED, 0xED, 0xF2));
            round_stroke_w(dc, &r, RADIUS_CHIP, rgb(0x2E, 0x2E, 0x36), 2);
        } else if hover == idx || pressed == idx {
            round_fill(
                dc,
                &rect(
                    b.rect.left + 2,
                    b.rect.top + 3,
                    b.rect.right - 2,
                    b.rect.bottom - 3,
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

unsafe fn draw_props(
    dc: HDC,
    props: &[(RECT, Prop)],
    hover_prop: i32,
    style: &Style,
    tool: Tool,
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
            let sep = rect(cx, r.top + 10, cx + 1, r.bottom - 10);
            FillRect(dc, &sep, b);
            DeleteObject(b as HGDIOBJ);
            continue;
        }

        if hover_prop == i as i32 {
            round_fill(
                dc,
                &rect(r.left + 2, r.top + 3, r.right - 2, r.bottom - 3),
                RADIUS_CHIP,
                rgb(0xF1, 0xF1, 0xF5),
            );
        }

        match *prop {
            Prop::Color(idx) => {
                let sel = style.color == COLORS[idx];
                let d = rect(cx - 9, cy - 9, cx + 9, cy + 9);
                round_fill(dc, &d, 8, COLORS[idx]);
                if sel {
                    round_stroke(
                        dc,
                        &rect(d.left - 3, d.top - 3, d.right + 3, d.bottom + 3),
                        10,
                        rgb(0x2A, 0x2A, 0x30),
                    );
                } else {
                    round_stroke(dc, &d, 8, rgb(0xD8, 0xD8, 0xDE));
                }
            }
            Prop::Width(idx) => {
                let sel = style.width == tool.width_at(idx);
                // 显示粗细按档位序号放大，跟实际像素脱钩，三档才分得开
                let shown = 2 + idx as i32 * 2;
                let c = if sel { ON } else { OFF };
                let pen = CreatePen(PS_SOLID, shown, rgb(c.0, c.1, c.2));
                let old = SelectObject(dc, pen as HGDIOBJ);
                MoveToEx(dc, cx - 9, cy, std::ptr::null_mut());
                LineTo(dc, cx + 9, cy);
                SelectObject(dc, old);
                DeleteObject(pen as HGDIOBJ);
                if sel {
                    round_stroke_w(
                        dc,
                        &rect(cx - 11, cy - 11, cx + 11, cy + 11),
                        8,
                        rgb(0x2E, 0x2E, 0x36),
                        2,
                    );
                }
            }
            Prop::Filled => {
                let b = rect(cx - 8, cy - 8, cx + 8, cy + 8);
                if style.filled {
                    round_fill(dc, &b, 4, rgb(ON.0, ON.1, ON.2));
                } else {
                    round_stroke(dc, &b, 4, rgb(OFF.0, OFF.1, OFF.2));
                }
            }
            Prop::Dashed => {
                let c = if style.dashed { ON } else { OFF };
                let pen = CreatePen(PS_SOLID, 2, rgb(c.0, c.1, c.2));
                let old = SelectObject(dc, pen as HGDIOBJ);
                if style.dashed {
                    for k in 0..3 {
                        let x0 = cx - 9 + k * 7;
                        MoveToEx(dc, x0, cy, std::ptr::null_mut());
                        LineTo(dc, x0 + 4, cy);
                    }
                } else {
                    MoveToEx(dc, cx - 9, cy, std::ptr::null_mut());
                    LineTo(dc, cx + 9, cy);
                }
                SelectObject(dc, old);
                DeleteObject(pen as HGDIOBJ);
            }
            Prop::Round(is_round) => {
                let sel = style.round == is_round;
                let c = if sel { ON } else { OFF };
                let pen = CreatePen(PS_SOLID, 2, rgb(c.0, c.1, c.2));
                let old_p = SelectObject(dc, pen as HGDIOBJ);
                let old_b = SelectObject(dc, GetStockObject(NULL_BRUSH) as HGDIOBJ);
                if is_round {
                    Ellipse(dc, cx - 8, cy - 8, cx + 8, cy + 8);
                } else {
                    Rectangle(dc, cx - 8, cy - 8, cx + 8, cy + 8);
                }
                SelectObject(dc, old_p);
                SelectObject(dc, old_b);
                DeleteObject(pen as HGDIOBJ);
                if sel {
                    round_stroke_w(
                        dc,
                        &rect(cx - 13, cy - 13, cx + 13, cy + 13),
                        10,
                        rgb(0x2E, 0x2E, 0x36),
                        2,
                    );
                }
            }
            Prop::Sep => {}
        }
    }

    round_stroke(dc, &bar, RADIUS_BAR, rgb(0xD6, 0xD6, 0xDC));
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
        BitBlt(dc, x - r, y - r, r * 2, r * 2, screen.hdc, x - r, y - r, SRCCOPY);
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
                let idx = ov.hit_button(x, y);
                let pidx = ov.hit_prop(x, y);

                if idx >= 0 {
                    ov.pressed = idx;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                } else if pidx >= 0 {
                    apply_prop(ov, pidx);
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                } else if ov.locked && ov.tool.is_some() {
                    if let Some(s) = ov.selection() {
                        if pt_in(&s, x, y) {
                            ov.stroke_begin(x, y);
                            SetCapture(hwnd);
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
                    ov.locked = true;
                    ReleaseCapture();
                    ov.layout_buttons();
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
                0
            }
            WM_RBUTTONUP => {
                ov.finish(CMD_CANCEL);
                0
            }
            WM_KEYDOWN => {
                let vk = wparam as u16 as i32;
                let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
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
            WM_PAINT => {
                ov.paint();
                0
            }
            windows_sys::Win32::UI::WindowsAndMessaging::WM_DESTROY => {
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
        Prop::Sep => {}
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
            styles: {
                let base = Style::default();
                let mut arrow = base;
                arrow.width = 4;
                let mut eraser = base;
                eraser.width = 24;
                eraser.round = true;
                [base, arrow, base, eraser]
            },
            draft: None,
            drawing: false,
            doc: Doc::default(),
            icons: crate::icons::load(),
            back_dc,
            back_bmp,
            mask_dc,
            mask_bmp,
            label_font: ui_font(12),
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
        }
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        SetFocus(hwnd);
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
