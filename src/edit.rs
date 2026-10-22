//! 标注元素：数据结构、GDI 绘制、撤销栈。
//!
//! 坐标一律相对选区左上角，绘制的时侯再加屏幕偏移 (ox, oy)。
//! 这样选区移动或者窗口尺寸变化都不影响已画好的元素。

use crate::gdiplus::FontSpec;
use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::Graphics::Gdi::{
    BeginPath, CreatePen, CreateSolidBrush, DeleteObject, Ellipse, EndPath, GetStockObject,
    Polyline, SelectObject, SetBkMode, SetTextColor, StrokePath, TextOutW, HDC, HGDIOBJ,
    NULL_BRUSH, NULL_PEN, PS_SOLID, TRANSPARENT,
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
    /// 文本：加粗
    pub bold: bool,
    /// 文本：描边（自动取对比色）
    pub stroke: bool,
    /// 文本：投影
    pub shadow: bool,
}

impl Style {
    /// 存成一行：`RRGGBB,线宽,标志位`。
    ///
    /// 标志位按位拼：1 虚线、2 填充、4 起点箭头、8 终点箭头、
    /// 16 圆角、32 加粗、64 描边、128 投影。
    /// 用逗号分隔的短串，配置文件里一眼能看懂，也能手改。
    pub fn to_token(&self) -> String {
        let (r, g, b) = (
            self.color & 0xFF,
            (self.color >> 8) & 0xFF,
            (self.color >> 16) & 0xFF,
        );
        let mut flags = 0u32;
        if self.dashed {
            flags |= 1;
        }
        if self.filled {
            flags |= 2;
        }
        if self.head {
            flags |= 4;
        }
        if self.tail {
            flags |= 8;
        }
        if self.round {
            flags |= 16;
        }
        if self.bold {
            flags |= 32;
        }
        if self.stroke {
            flags |= 64;
        }
        if self.shadow {
            flags |= 128;
        }
        format!("{r:02X}{g:02X}{b:02X},{},{}", self.width, flags)
    }

    pub fn from_token(s: &str) -> Option<Style> {
        let mut it = s.split(',');
        let hex = it.next()?.trim();
        let width: i32 = it.next()?.trim().parse().ok()?;
        let flags: u32 = it.next().unwrap_or("0").trim().parse().unwrap_or(0);
        if hex.len() != 6 {
            return None;
        }
        let r = u32::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u32::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u32::from_str_radix(&hex[4..6], 16).ok()?;
        Some(Style {
            color: r | (g << 8) | (b << 16),
            width: width.clamp(1, 200),
            dashed: flags & 1 != 0,
            filled: flags & 2 != 0,
            head: flags & 4 != 0,
            tail: flags & 8 != 0,
            round: flags & 16 != 0,
            bold: flags & 32 != 0,
            stroke: flags & 64 != 0,
            shadow: flags & 128 != 0,
        })
    }
}

impl Default for Style {
    fn default() -> Self {
        Self {
            color: 0x0063_2EFF, // #FF2E63 的 COLORREF 写法
            width: 4,
            dashed: false,
            filled: false,
            head: true,
            tail: false,
            round: false,
            bold: false,
            stroke: false,
            shadow: false,
        }
    }
}

#[derive(Clone)]
pub enum Kind {
    Arrow {
        a: POINT,
        b: POINT,
    },
    /// 矩形或椭圆，看 style.round
    Box {
        a: POINT,
        b: POINT,
    },
    Pen {
        pts: Vec<POINT>,
    },
    /// 橡皮：重绘时把底图这一块盖回来，不走常规描边
    Eraser {
        pts: Vec<POINT>,
    },
    Text {
        at: POINT,
        text: String,
        /// 输入时 EDIT 用的行高（GDI 的 tmHeight）。0 表示按字号估算。
        line_h: i32,
    },
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

    /// 整体平移所有元素。
    ///
    /// 元素坐标是相对选区左上角存的，选区一移动/改左边界，
    /// 就得反向平移，标注才会钉在屏幕原处而不是跟着选区跑。
    pub fn shift(&mut self, dx: i32, dy: i32) {
        if dx == 0 && dy == 0 {
            return;
        }
        for s in &mut self.shapes {
            match &mut s.kind {
                Kind::Arrow { a, b } | Kind::Box { a, b } => {
                    a.x += dx;
                    a.y += dy;
                    b.x += dx;
                    b.y += dy;
                }
                Kind::Pen { pts } | Kind::Eraser { pts } => {
                    for p in pts {
                        p.x += dx;
                        p.y += dy;
                    }
                }
                Kind::Text { at, .. } => {
                    at.x += dx;
                    at.y += dy;
                }
            }
        }
    }
}

// ---------------------------------------------------------------- 绘制

pub fn draw(dc: HDC, s: &Shape, ox: i32, oy: i32) {
    match &s.kind {
        Kind::Arrow { a, b } => unsafe { arrow(dc, &s.style, *a, *b, ox, oy) },
        Kind::Box { a, b } => unsafe { box_shape(dc, &s.style, *a, *b, ox, oy) },
        Kind::Pen { pts } => unsafe { pen_stroke(dc, &s.style, pts, ox, oy) },
        Kind::Eraser { .. } => {} // 由 overlay 特判
        Kind::Text { at, text, line_h } => unsafe {
            label(dc, &s.style, *at, text, *line_h, ox, oy)
        },
    }
}

unsafe fn arrow(dc: HDC, st: &Style, a: POINT, b: POINT, ox: i32, oy: i32) {
    let (x0, y0) = (a.x + ox, a.y + oy);
    let (x1, y1) = (b.x + ox, b.y + oy);
    let dx = (x1 - x0) as f32;
    let dy = (y1 - y0) as f32;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1.0 {
        return;
    }
    let (ux, uy) = (dx / len, dy / len);
    let head = head_len(st);

    // 太短就只画个头
    if len < head * 1.2 {
        if st.head {
            head_tri(dc, st, (x0, y0), (x1, y1));
        }
        return;
    }

    // 线身两端各让出箭头长度的一大半。
    // 不让的话，方头笔帽会从三角尖端捅出去，箭尖看起来是平的。
    let back = if st.head { head * 0.8 } else { 0.0 };
    let front = if st.tail { head * 0.8 } else { 0.0 };
    stroke_aa(
        dc,
        st,
        x0 + (ux * front) as i32,
        y0 + (uy * front) as i32,
        x1 - (ux * back) as i32,
        y1 - (uy * back) as i32,
    );

    if st.head {
        head_tri(dc, st, (x0, y0), (x1, y1));
    }
    if st.tail {
        head_tri(dc, st, (x1, y1), (x0, y0));
    }
}

/// 箭头长度：跟着线宽走，但别小于 11px，否则看不见
fn head_len(st: &Style) -> f32 {
    (st.width as f32 * 4.2).max(11.0)
}

/// 一条线。虚线自己切段，每段都走 GDI+，边缘才平滑。
unsafe fn stroke_aa(dc: HDC, st: &Style, x0: i32, y0: i32, x1: i32, y1: i32) {
    let w = st.width.max(1) as f32;

    if st.dashed {
        let dx = (x1 - x0) as f32;
        let dy = (y1 - y0) as f32;
        let len = (dx * dx + dy * dy).sqrt();
        if len >= 1.0 {
            let (ux, uy) = (dx / len, dy / len);
            let dash = (w * 4.0).max(6.0);
            let gap = (w * 3.0).max(4.0);
            let mut pos = 0.0f32;
            while pos < len {
                let end = (pos + dash).min(len);
                crate::gdiplus::line(
                    dc,
                    (x0 as f32 + ux * pos) as i32,
                    (y0 as f32 + uy * pos) as i32,
                    (x0 as f32 + ux * end) as i32,
                    (y0 as f32 + uy * end) as i32,
                    st.color,
                    w,
                );
                pos = end + gap;
            }
        }
    } else {
        crate::gdiplus::line(dc, x0, y0, x1, y1, st.color, w);
    }
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
    let size = head_len(st);
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

    crate::gdiplus::fill_polygon(dc, &pts, st.color);
}

unsafe fn box_shape(dc: HDC, st: &Style, a: POINT, b: POINT, ox: i32, oy: i32) {
    let l = a.x.min(b.x) + ox;
    let t = a.y.min(b.y) + oy;
    let r = a.x.max(b.x) + ox;
    let btm = a.y.max(b.y) + oy;
    if r - l < 2 || btm - t < 2 {
        return;
    }

    let full = crate::win::rect(l, t, r, btm);
    let w = st.width.max(1) as f32;
    // GDI+ 的画笔以路径中心线为准，往里收半个线宽，视觉外边界才和拖拽框一致
    let half = (w * 0.5).round() as i32;
    let stroke_r = if r - l > half * 2 + 1 && btm - t > half * 2 + 1 {
        crate::win::rect(l + half, t + half, r - half, btm - half)
    } else {
        full
    };

    if st.filled {
        if st.round {
            crate::gdiplus::fill_ellipse(dc, &full, st.color);
        } else {
            crate::gdiplus::rect_fill(dc, &full, st.color);
        }
    }
    if st.round {
        crate::gdiplus::ellipse(dc, &stroke_r, st.color, w);
    } else {
        crate::gdiplus::rect_stroke(dc, &stroke_r, st.color, w);
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

/// 文本：先投影，再描边，最后填字。
///
/// 三层顺序不能反——描边是沿字形轮廓勾的，压在里面会被填充盖掉一半。
unsafe fn label(dc: HDC, st: &Style, at: POINT, text: &str, line_h: i32, ox: i32, oy: i32) {
    if text.is_empty() {
        return;
    }
    // 文本工具把字号存在 width 里（属性条的 Width 档位就是字号）
    let px = st.width.max(8);
    let spec = FontSpec::new(px as f32, st.bold, "Microsoft YaHei");
    // GDI+ 的 DrawString 在布局矩形里自带约 1/6 em 的左内边距，
    // 而输入框的 EDIT 是从边距处起笔的。不补掉这个差值，提交瞬间整段字会右移。
    // GDI 绘制时，文字原点和 EDIT 内部缩进对齐即可（实测 2px）
    let pad_x = 2;
    let pad_y = 3;
    let x = at.x + ox - pad_x;
    let lh = if line_h > 0 {
        line_h
    } else {
        (px as f32 * 1.35) as i32
    };

    // 逐行画：行距必须用输入时 EDIT 的 tmHeight，否则多行会越写越偏
    for (i, line) in text.split('\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let y = at.y + oy + i as i32 * lh - pad_y;
        if st.shadow {
            crate::gdiplus::draw_text_argb(dc, &spec, 0x6600_0000, x + 2, y + 2, line);
        }
        let font = crate::win::ui_font_w(px, if st.bold { 700 } else { 400 });
        let old_f = SelectObject(dc, font as HGDIOBJ);
        let wide: Vec<u16> = line.encode_utf16().collect();
        SetBkMode(dc, TRANSPARENT as i32);

        if st.stroke {
            // 描边也走 GDI：把同一份字形轮廓转成路径再勾边，
            // 和下面的填充共用一套 glyph，不会出现描边和字心错半个像素。
            let w = (px as f32 / 9.0).max(2.0) as i32;
            let pen = CreatePen(PS_SOLID, w, contrast_color(st.color));
            let old_p = SelectObject(dc, pen as HGDIOBJ);
            let old_b = SelectObject(dc, GetStockObject(NULL_BRUSH) as HGDIOBJ);
            BeginPath(dc);
            TextOutW(dc, x, y, wide.as_ptr(), wide.len() as i32);
            EndPath(dc);
            StrokePath(dc);
            SelectObject(dc, old_p);
            SelectObject(dc, old_b);
            DeleteObject(pen as HGDIOBJ);
        }

        SetTextColor(dc, st.color);
        TextOutW(dc, x, y, wide.as_ptr(), wide.len() as i32);
        SelectObject(dc, old_f);
        DeleteObject(font as HGDIOBJ);
    }
}

/// 描边取反差色：深字配白边，浅字配黑边
fn contrast_color(c: u32) -> u32 {
    let r = (c & 0xFF) as f32;
    let g = ((c >> 8) & 0xFF) as f32;
    let b = ((c >> 16) & 0xFF) as f32;
    if 0.299 * r + 0.587 * g + 0.114 * b > 150.0 {
        0x0000_0000
    } else {
        0x00FF_FFFF
    }
}
