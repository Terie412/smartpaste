//! 标注元素：数据结构、GDI 绘制、撤销栈。
//!
//! 坐标一律相对选区左上角，绘制的时侯再加屏幕偏移 (ox, oy)。
//! 这样选区移动或者窗口尺寸变化都不影响已画好的元素。

use crate::win::ui_font;
use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::Graphics::Gdi::{
    CreatePen, CreateSolidBrush, DeleteObject, Ellipse, GetStockObject, LineTo, MoveToEx, Polygon,
    Polyline, Rectangle, SelectObject, SetBkMode, SetTextColor, TextOutW, HDC, HGDIOBJ, NULL_BRUSH,
    NULL_PEN, PS_SOLID, TRANSPARENT,
};

/// 描边样式。颜色是 COLORREF（0x00BBGGRR）。
#[derive(Clone, Copy)]
pub struct Style {
    pub color: u32,
    pub width: i32,
    pub dashed: bool,
    pub filled: bool,
    /// 终点带箭头
    pub head: bool,
    /// 起点也带箭头（双向）
    pub tail: bool,
    /// 形状工具：true = 圆/椭圆，false = 矩形
    /// 橡皮工具：true = 圆笔刷，false = 方笔刷
    pub round: bool,
    pub font_size: i32,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            color: 0x0063_2EFF, // #FF2E63 的 COLORREF 写法
            width: 3,
            dashed: false,
            filled: false,
            head: true,
            tail: false,
            round: false,
            font_size: 22,
        }
    }
}

#[derive(Clone)]
pub enum Kind {
    Arrow { a: POINT, b: POINT },
    /// 矩形或椭圆，看 style.round
    Box { a: POINT, b: POINT },
    Pen { pts: Vec<POINT> },
    /// 橡皮：重绘时把底图这一块盖回来，不走常规描边
    Eraser { pts: Vec<POINT> },
    Text { at: POINT, text: String },
}

#[derive(Clone)]
pub struct Shape {
    pub kind: Kind,
    pub style: Style,
}

#[derive(Default)]
pub struct Doc {
    pub shapes: Vec<Shape>,
    redo: Vec<Shape>,
}

impl Doc {
    pub fn push(&mut self, s: Shape) {
        self.shapes.push(s);
        self.redo.clear();
    }

    pub fn undo(&mut self) -> bool {
        match self.shapes.pop() {
            Some(s) => {
                self.redo.push(s);
                true
            }
            None => false,
        }
    }

    pub fn redo(&mut self) -> bool {
        match self.redo.pop() {
            Some(s) => {
                self.shapes.push(s);
                true
            }
            None => false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.shapes.is_empty()
    }
}

// ---------------------------------------------------------------- 绘制

pub fn draw(dc: HDC, s: &Shape, ox: i32, oy: i32) {
    match &s.kind {
        Kind::Arrow { a, b } => unsafe { arrow(dc, &s.style, *a, *b, ox, oy) },
        Kind::Box { a, b } => unsafe { box_shape(dc, &s.style, *a, *b, ox, oy) },
        Kind::Pen { pts } => unsafe { pen_stroke(dc, &s.style, pts, ox, oy) },
        Kind::Eraser { .. } => {} // 由 overlay 特判
        Kind::Text { at, text } => unsafe { label(dc, &s.style, *at, text, ox, oy) },
    }
}

unsafe fn arrow(dc: HDC, st: &Style, a: POINT, b: POINT, ox: i32, oy: i32) {
    let (x0, y0) = (a.x + ox, a.y + oy);
    let (x1, y1) = (b.x + ox, b.y + oy);
    stroke(dc, st, x0, y0, x1, y1);
    if st.head {
        head_tri(dc, st, (x0, y0), (x1, y1));
    }
    if st.tail {
        head_tri(dc, st, (x1, y1), (x0, y0));
    }
}

/// 一条线：实线走 GDI，虚线自己切段（GDI 的 PS_DASH 只对 1px 有效）。
unsafe fn stroke(dc: HDC, st: &Style, x0: i32, y0: i32, x1: i32, y1: i32) {
    let w = st.width.max(1);
    let pen = CreatePen(PS_SOLID, w, st.color);
    let old = SelectObject(dc, pen as HGDIOBJ);

    if st.dashed {
        let dx = (x1 - x0) as f32;
        let dy = (y1 - y0) as f32;
        let len = (dx * dx + dy * dy).sqrt();
        if len >= 1.0 {
            let (ux, uy) = (dx / len, dy / len);
            let dash = (w as f32 * 4.0).max(6.0);
            let gap = (w as f32 * 3.0).max(4.0);
            let mut pos = 0.0f32;
            while pos < len {
                let end = (pos + dash).min(len);
                MoveToEx(dc, x0 + (ux * pos) as i32, y0 + (uy * pos) as i32, std::ptr::null_mut());
                LineTo(dc, x0 + (ux * end) as i32, y0 + (uy * end) as i32);
                pos = end + gap;
            }
        }
    } else {
        MoveToEx(dc, x0, y0, std::ptr::null_mut());
        LineTo(dc, x1, y1);
    }

    SelectObject(dc, old);
    DeleteObject(pen as HGDIOBJ);
}

/// 箭头尖：实心三角，大小跟着线宽走
unsafe fn head_tri(dc: HDC, st: &Style, from: (i32, i32), to: (i32, i32)) {
    let dx = (to.0 - from.0) as f32;
    let dy = (to.1 - from.1) as f32;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 2.0 {
        return;
    }
    let (ux, uy) = (dx / len, dy / len);
    let size = (st.width as f32 * 4.2).max(11.0);
    let (px, py) = (-uy, ux);
    let bx = to.0 - (ux * size) as i32;
    let by = to.1 - (uy * size) as i32;
    let spread = size * 0.42;

    let pts = [
        POINT { x: to.0, y: to.1 },
        POINT {
            x: bx + (px * spread) as i32,
            y: by + (py * spread) as i32,
        },
        POINT {
            x: bx - (px * spread) as i32,
            y: by - (py * spread) as i32,
        },
    ];

    let brush = CreateSolidBrush(st.color);
    let old_b = SelectObject(dc, brush as HGDIOBJ);
    let old_p = SelectObject(dc, GetStockObject(NULL_PEN) as HGDIOBJ);
    Polygon(dc, pts.as_ptr(), 3);
    SelectObject(dc, old_b);
    SelectObject(dc, old_p);
    DeleteObject(brush as HGDIOBJ);
}

unsafe fn box_shape(dc: HDC, st: &Style, a: POINT, b: POINT, ox: i32, oy: i32) {
    let l = a.x.min(b.x) + ox;
    let t = a.y.min(b.y) + oy;
    let r = a.x.max(b.x) + ox;
    let btm = a.y.max(b.y) + oy;
    if r - l < 2 || btm - t < 2 {
        return;
    }

    let pen = CreatePen(PS_SOLID, st.width.max(1), st.color);
    let brush = if st.filled {
        CreateSolidBrush(st.color)
    } else {
        GetStockObject(NULL_BRUSH)
    };
    let old_p = SelectObject(dc, pen as HGDIOBJ);
    let old_b = SelectObject(dc, brush as HGDIOBJ);

    if st.round {
        Ellipse(dc, l, t, r, btm);
    } else {
        Rectangle(dc, l, t, r, btm);
    }

    SelectObject(dc, old_p);
    SelectObject(dc, old_b);
    DeleteObject(pen as HGDIOBJ);
    if st.filled {
        DeleteObject(brush as HGDIOBJ);
    }
}

unsafe fn pen_stroke(dc: HDC, st: &Style, pts: &[POINT], ox: i32, oy: i32) {
    if pts.len() < 2 {
        if let Some(p) = pts.first() {
            // 单点也要留个印子
            let brush = CreateSolidBrush(st.color);
            let old_b = SelectObject(dc, brush as HGDIOBJ);
            let old_p = SelectObject(dc, GetStockObject(NULL_PEN) as HGDIOBJ);
            let r = st.width.max(2);
            Ellipse(dc, p.x + ox - r, p.y + oy - r, p.x + ox + r, p.y + oy + r);
            SelectObject(dc, old_b);
            SelectObject(dc, old_p);
            DeleteObject(brush as HGDIOBJ);
        }
        return;
    }

    let shifted: Vec<POINT> = pts
        .iter()
        .map(|p| POINT {
            x: p.x + ox,
            y: p.y + oy,
        })
        .collect();
    let pen = CreatePen(PS_SOLID, st.width.max(1), st.color);
    let old = SelectObject(dc, pen as HGDIOBJ);
    Polyline(dc, shifted.as_ptr(), shifted.len() as i32);
    SelectObject(dc, old);
    DeleteObject(pen as HGDIOBJ);
}

unsafe fn label(dc: HDC, st: &Style, at: POINT, text: &str, ox: i32, oy: i32) {
    if text.is_empty() {
        return;
    }
    let font = ui_font(st.font_size.max(8));
    let old_f = SelectObject(dc, font as HGDIOBJ);
    let wide: Vec<u16> = text.encode_utf16().collect();

    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(dc, st.color);
    TextOutW(
        dc,
        at.x + ox,
        at.y + oy,
        wide.as_ptr(),
        wide.len() as i32,
    );

    SelectObject(dc, old_f);
    DeleteObject(font as HGDIOBJ);
}
