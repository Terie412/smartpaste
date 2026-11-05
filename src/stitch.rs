//! 滚动截图：把一帧帧滚动的画面拼成一张长图。
//!
//! 核心问题只有一个——"这一帧相对上一帧移动了多少像素、朝哪个方向"。这个数字
//! 拿不到，因为每个程序一次滚轮滚多少完全不一样（浏览器按行、编辑器按像素、有的
//! 还带惯性动画）。所以只能从画面里反推：拿当前帧的每一行去上一帧里找，重合最多、
//! 且重合部分严丝合缝的那个位移就是答案。
//!
//! **两个方向都支持**：往下滚时新内容出现在底部（拼到画布末尾），往上滚时新内容
//! 出现在顶部（拼到画布开头）。画布按"段"存（`VecDeque<Vec<u8>>`），两头 push 都
//! 是摊还 O(1)，不用为了往前插而整体挪动几十兆像素；只有出图时合并一次。
//!
//! 三个会导致"拼歪/重影"的坑都在这里处理：
//!
//! 1. **纯色行会匹配一切**。一整片背景色在哪个位移上都对得上，只能把它从
//!    打分里剔掉，只信"有内容"的行。
//! 2. **吸顶表头 / 吸底状态栏每帧都不动**。它们的位移恒为 0，混进对齐会
//!    把结果往 0 拉。先识别出上下两条静止带，既不参与对齐，也不重复拼进去。
//! 3. **重复内容会给出多个同样好的位移**（等距列表、纯文字块）。这时取离
//!    "上一次位移"最近的那个——滚动是连续的，跳变才是异常。
//!
//! 判定用了量化容差（每通道右移 3 位）：同一段内容在两帧里可能因为子像素
//! 渲染差一两级，逐字节比会把它判成"变了"。

use image::RgbaImage;
use std::collections::VecDeque;

/// 行指纹的采样列数上限。
const SAMPLES: u32 = 160;
/// 每通道量化位数：右移 3 位 = 32 档，约 ±7 的差异算同一行。
const QUANT: u32 = 3;
/// 一次对齐至少要这么多"有内容"的行重合，否则宁可判失败。
const MIN_ROWS: u32 = 24;
/// 小于这个位移当抖动，不拼——避免 1px 的抖动把图撑长。
const MIN_SHIFT: u32 = 2;
/// 两帧的行相同比例超过它就算"没动过"。
const SAME_RATIO: f32 = 0.98;
/// 对齐时允许的错行比例：视频、光标闪烁会造成少量行变化。
const MATCH_RATIO: f32 = 0.94;
/// 粗筛时每个候选位移抽查的行数。
const PROBE_ROWS: u32 = 32;
/// 行指纹只取中间这一点宽度，两边各切掉这么多百分比。
const SIDE_CUT: u32 = 15;

/// 一行像素的指纹
type Sig = u64;

/// 一帧画面 + 预先算好的行指纹。
///
/// 指纹算一次要用两次（判断"有没有动"和"动了多少"），所以单独抽出来。
pub struct Frame {
    pub w: u32,
    pub h: u32,
    raw: Vec<u8>,
    sig: Vec<Sig>,
    /// 这一行是纯色（采样点几乎没差异），打分时跳过
    plain: Vec<bool>,
}

impl Frame {
    pub fn new(img: RgbaImage) -> Self {
        let (w, h) = img.dimensions();
        let raw = img.into_raw();
        let row_len = (w * 4) as usize;
        let usable = w - w * SIDE_CUT / 100 * 2;
        let step = (usable.max(1) / SAMPLES).max(1);

        let mut sig = Vec::with_capacity(h as usize);
        let mut plain = Vec::with_capacity(h as usize);
        for y in 0..h as usize {
            let row = &raw[y * row_len..(y + 1) * row_len];
            let (s, flat) = row_sig(row, w, step);
            sig.push(s);
            plain.push(flat);
        }
        Self {
            w,
            h,
            raw,
            sig,
            plain,
        }
    }
}

fn row_sig(row: &[u8], w: u32, step: u32) -> (Sig, bool) {
    let mut hash: Sig = 0xcbf2_9ce4_8422_2325;
    let mut lo = u8::MAX;
    let mut hi = 0u8;
    // 只取中间 70%：选区的左右边缘经常混进固定不动的东西（浏览器侧栏、滚动条、
    // 页边距），它们不跟着内容滚，一行里只要有一小段不对，整行哈希就废了。
    let x0 = w * SIDE_CUT / 100;
    let x1 = w - x0;
    let mut x = x0;
    while x < x1 {
        let i = (x * 4) as usize;
        let r = row[i] >> QUANT;
        let g = row[i + 1] >> QUANT;
        let b = row[i + 2] >> QUANT;
        lo = lo.min(r).min(g).min(b);
        hi = hi.max(r).max(g).max(b);
        // 15 位打包再混合：不同颜色不会被哈希碰巧抹平
        let v = (r as Sig) | ((g as Sig) << 5) | ((b as Sig) << 10);
        hash ^= v;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        x += step;
    }
    (hash, hi - lo <= 1)
}

/// 两帧像不像（只看行指纹，不看原始像素）。
pub fn similar(a: &Frame, b: &Frame) -> bool {
    if a.h != b.h || a.w != b.w {
        return false;
    }
    let mut same = 0;
    let mut total = 0;
    for y in 0..a.h as usize {
        if a.plain[y] && b.plain[y] {
            continue;
        }
        total += 1;
        if a.sig[y] == b.sig[y] {
            same += 1;
        }
    }
    total == 0 || same as f32 / total as f32 >= SAME_RATIO
}

/// 提交一帧的结果
#[derive(Debug, PartialEq, Eq)]
pub enum Push {
    /// 内容往上走（用户往下滚）：拼进去了 n 行，接在末尾
    Appended(u32),
    /// 内容往下走（用户往上滚）：拼进去了 n 行，接在开头
    Prepended(u32),
    /// 和上一帧一样：没滚，或者滚到头了
    Same,
    /// 对不上（画面在动、被遮挡、或者干脆换了内容），这帧丢了
    Rejected,
    /// 到尺寸上限了
    Full,
}

pub struct Stitcher {
    w: u32,
    frame_h: u32,
    /// 已拼好的内容，按从上到下排列。往下滚 push_back、往上滚 push_front，
    /// 两个方向都是摊还 O(1)，不会为了往前插而搬整个画布。
    segs: VecDeque<Vec<u8>>,
    /// 已拼进去的总行数（含吸顶表头那一份）
    rows: u32,
    /// 最前面那几行是吸顶表头。往上滚时新内容要插在它**后面**，
    /// 而不是盖到它上面——表格头本来就该永远在最顶上。
    header_rows: u32,
    /// 当前这一帧的**内容带顶行**落在内容坐标的哪一行。往回滚时靠它判断
    /// "这块其实已经有了"。
    origin: i64,
    /// 画布第一行对应的内容坐标。往上拼几段之后它会变成负数——
    /// 只看 `rows`（行数）是不够的，必须知道画布到底覆盖了哪一段。
    top_row: i64,
    max_rows: u32,
    prev: Option<Frame>,
    /// 上一次成功对齐的位移。正 = 内容上移（向下滚），负 = 内容下移（向上滚）。
    /// 重复内容靠它挑候选。
    last_shift: Option<i32>,
    /// 上下两条"静止带"（吸顶表头、吸底状态栏）的高度，取见过的最大值
    top: u32,
    bottom: u32,
    /// 已接受的帧数
    pub frames: u32,
}

impl Stitcher {
    /// `max_rows` 是画布上限（行数）。宽度越大越早停，避免几亿字节的 Vec。
    pub fn new(w: u32, frame_h: u32, max_rows: u32) -> Self {
        Self {
            w,
            frame_h,
            segs: VecDeque::new(),
            rows: 0,
            header_rows: 0,
            origin: 0,
            top_row: 0,
            max_rows,
            prev: None,
            last_shift: None,
            top: 0,
            bottom: 0,
            frames: 0,
        }
    }

    pub fn width(&self) -> u32 {
        self.w
    }

    pub fn height(&self) -> u32 {
        self.rows
    }

    /// 内容行数（不含吸顶表头）
    fn content_rows(&self) -> u32 {
        self.rows - self.header_rows
    }

    pub fn push(&mut self, frame: Frame) -> Push {
        if frame.w != self.w {
            return Push::Rejected;
        }
        if self.rows + self.frame_h > self.max_rows && self.frames > 0 {
            return Push::Full;
        }

        if self.prev.is_none() {
            // 第一帧整帧铺上。这时候还不知道有没有吸顶表头，等第二帧再切。
            self.rows = frame.h;
            self.segs.push_back(frame.raw.clone());
            self.frames = 1;
            self.origin = 0;
            self.top_row = 0;
            self.prev = Some(frame);
            return Push::Appended(self.rows);
        }

        // 借用 self.prev 的每一段都单独收在块里：后面要调 &mut self 的方法（切第一帧），
        // 同时持有不可变借用会编译不过。
        let same = {
            let prev = self.prev.as_ref().unwrap();
            similar(prev, &frame)
        };
        if same {
            return Push::Same;
        }

        // 静止带：上下各有多少行和上一帧一模一样。吸顶表头就靠这个识别。
        let (top, bottom) = {
            let prev = self.prev.as_ref().unwrap();
            (leading_equal(prev, &frame), trailing_equal(prev, &frame))
        };
        if top > self.top {
            self.top = top;
        }
        if bottom > self.bottom {
            self.bottom = bottom;
        }
        if self.top + self.bottom + MIN_ROWS * 2 >= frame.h {
            return Push::Rejected;
        }
        // 第一次识别出静止带时，把第一帧里这两条带切出去：
        // 表头单独留在最前面，表尾直接丢掉（它只是视口里不动的一条，不是内容）
        if self.frames == 1 && self.header_rows == 0 && top > 0 {
            self.split_first_frame(top);
        }

        let shift = {
            let prev = self.prev.as_ref().unwrap();
            find_shift(prev, &frame, self.top, self.bottom, self.last_shift)
        };
        let Some(shift) = shift else {
            return Push::Rejected;
        };
        let d = shift.unsigned_abs();
        if d < MIN_SHIFT {
            return Push::Same;
        }

        let row_len = (self.w * 4) as usize;
        let band = (frame.h - self.top - self.bottom) as i64; // 内容带高度
        // 往下滚（shift > 0）内容往上走：当前帧的顶行对应画布更靠下的位置
        let next_origin = self.origin + shift as i64;
        // 画布覆盖 [top_row, top_row + content_rows)
        let canvas_end = self.top_row + self.content_rows() as i64;

        let below = next_origin + band - canvas_end; // > 0 说明下方有新内容
        let above = self.top_row - next_origin; // > 0 说明上方有新内容

        if below <= 0 && above <= 0 {
            // 又滚回了已经拼过的范围：什么都不用加，但要把"当前帧"推进，
            // 否则下一帧会拿老的那一帧去比
            self.origin = next_origin;
            self.last_shift = Some(shift);
            self.prev = Some(frame);
            return Push::Same;
        }

        let room = self.max_rows.saturating_sub(self.rows);
        if room == 0 {
            return Push::Full;
        }

        let (need, from, push_back) = if below > 0 {
            // 新增的是内容带底部那几行
            let need = below as u32;
            let from = (frame.h - self.bottom) as i64 - need as i64;
            (need, from as usize, true)
        } else {
            let need = above as u32;
            (need, self.top as usize, false)
        };
        let take = need.min(room);
        let start = from * row_len;
        let end = start + take as usize * row_len;
        let part = frame.raw[start..end].to_vec();
        if push_back {
            self.segs.push_back(part);
        } else {
            // 插在吸顶表头**后面**
            let at = if self.header_rows > 0 { 1 } else { 0 };
            self.segs.insert(at, part);
            self.top_row -= take as i64;
        }

        self.rows += take;
        self.origin = next_origin;
        self.frames += 1;
        self.last_shift = Some(shift);
        self.prev = Some(frame);

        if take < need {
            Push::Full
        } else if push_back {
            Push::Appended(take)
        } else {
            Push::Prepended(take)
        }
    }

    /// 把第一帧切成"表头 + 内容"，顺便丢掉表尾。
    ///
    /// 只在第二次提交、第一次识别出静止带时调用一次。切完之后 `origin` 归零
    /// （内容坐标从内容带顶行开始算）。
    fn split_first_frame(&mut self, top: u32) {
        let row_len = (self.w * 4) as usize;
        let Some(first) = self.segs.pop_front() else {
            return;
        };
        let first_rows = (first.len() / row_len) as u32;
        let bottom = self.bottom.min(first_rows.saturating_sub(top));
        let top_bytes = top as usize * row_len;
        let bottom_cut = (first_rows - bottom) as usize * row_len;
        if top_bytes >= bottom_cut {
            self.segs.push_front(first);
            return;
        }
        let header = first[..top_bytes].to_vec();
        let content = first[top_bytes..bottom_cut].to_vec();
        self.header_rows = top;
        // rows 是总行数（含表头），这里只丢表尾；表头挪进 header_rows 单独记，
        // 再从 rows 里减一次就重复扣了
        self.rows -= bottom;
        self.origin = 0;
        self.segs.push_front(content);
        self.segs.push_front(header);
    }

    /// 拿成品。各段在这里合并一次，`Vec<u8>` 所有权直接交给 `RgbaImage`，
    /// 不再复制第二份（长图动辄上亿字节，复制一次就是几百 MB 的峰值）。
    pub fn finish(self) -> Option<RgbaImage> {
        if self.rows == 0 {
            return None;
        }
        let row_len = (self.w * 4) as usize;
        let mut canvas = Vec::with_capacity(self.rows as usize * row_len);
        for seg in &self.segs {
            canvas.extend_from_slice(seg);
        }
        canvas.truncate(self.rows as usize * row_len);
        RgbaImage::from_raw(self.w, self.rows, canvas)
    }

    /// 把当前画布**最近邻**采样成 `pw × ph` 的 BGRA 位图，供预览窗口直接贴屏。
    ///
    /// 写 BGRA 而不是 RGBA，是因为唯一的使用方是 32bpp DIB（内存里就是 B,G,R,X）。
    /// 采样而不是缩放，是为了每帧只读 `pw × ph` 个像素——预览才 300×600 级别，
    /// 比把几十兆的画布整块缩一遍便宜两个数量级。
    pub fn sample_bgra(&self, dst: &mut [u8], pw: u32, ph: u32) {
        if pw == 0 || ph == 0 || self.rows == 0 || self.segs.is_empty() {
            return;
        }
        let row_len = (self.w * 4) as usize;
        // 段是按顺序排的，预览行号单调递增，所以整趟只往前走一次
        let mut seg_i = 0usize;
        let mut seg_start = 0u32; // 当前段的第一行在整张图里的行号
        let mut seg_rows = (self.segs[0].len() / row_len) as u32;

        for py in 0..ph {
            let sy = (py as u64 * self.rows as u64 / ph as u64) as u32;
            while seg_i + 1 < self.segs.len() && sy >= seg_start + seg_rows {
                seg_start += seg_rows;
                seg_i += 1;
                seg_rows = (self.segs[seg_i].len() / row_len) as u32;
            }
            let seg = &self.segs[seg_i];
            let local = (sy.saturating_sub(seg_start)) as usize * row_len;
            let out = py as usize * pw as usize * 4;
            for px in 0..pw as usize {
                let sx = (px as u64 * self.w as u64 / pw as u64) as usize * 4;
                let s = local + sx;
                let d = out + px * 4;
                dst[d] = seg[s + 2];
                dst[d + 1] = seg[s + 1];
                dst[d + 2] = seg[s];
                dst[d + 3] = 255;
            }
        }
    }
}

/// 开头有多少行和上一帧完全一样（吸顶带）。上限 h/3，防止纯色页面整帧同色。
fn leading_equal(prev: &Frame, cur: &Frame) -> u32 {
    let cap = cur.h / 3;
    let mut n = 0;
    while n < cap && prev.sig[n as usize] == cur.sig[n as usize] {
        n += 1;
    }
    if n < 2 {
        0
    } else {
        n
    }
}

/// 结尾有多少行和上一帧完全一样（吸底带）
fn trailing_equal(prev: &Frame, cur: &Frame) -> u32 {
    let cap = cur.h / 3;
    let mut n = 0;
    while n < cap {
        let y = cur.h - 1 - n;
        if prev.sig[y as usize] != cur.sig[y as usize] {
            break;
        }
        n += 1;
    }
    if n < 2 {
        0
    } else {
        n
    }
}

/// 找出这一帧相对上一帧移了多少、朝哪边。
///
/// 返回值约定：**正数 = 内容上移**（用户往下滚，新内容在底部），
/// **负数 = 内容下移**（用户往上滚，新内容在顶部）。
///
/// 两个方向都要试，因为从画面本身看不出用户是往哪边滚的：
///   - 往下滚：`cur[y] == prev[y + d]`
///   - 往上滚：`cur[y + d] == prev[y]`
/// 先按 32 行抽样粗筛，再对胜出的候选逐行复核——全量比对 × 上千个候选位移
/// 每帧都要跑，太浪费。
fn find_shift(prev: &Frame, cur: &Frame, top: u32, bottom: u32, hint: Option<i32>) -> Option<i32> {
    let lo = top;
    let hi = cur.h.saturating_sub(bottom);
    if hi <= lo + MIN_ROWS {
        return None;
    }
    let max_d = hi - lo - MIN_ROWS;
    if max_d < MIN_SHIFT {
        return None;
    }

    let mut best_score = 0.0f32;
    let mut cands: Vec<i32> = Vec::new();

    for d in MIN_SHIFT..=max_d {
        let end = hi - d;
        if end <= lo {
            break;
        }
        // 同一位移的两个方向各打一次分
        for shift in [d as i32, -(d as i32)] {
            let score = probe(prev, cur, lo, end, shift);
            if score > best_score + 0.001 {
                best_score = score;
                cands.clear();
                cands.push(shift);
            } else if (score - best_score).abs() <= 0.001 && score > 0.0 {
                cands.push(shift);
            }
        }
    }

    if best_score < 0.75 {
        return None;
    }

    // 多个候选一样好 = 遇到了重复内容（或者两个方向都对得上）。
    // 滚动是连续的，挑离上次位移最近的那个；第一次没有 hint 就挑位移最小的。
    let pick = match hint {
        Some(h) => *cands
            .iter()
            .min_by_key(|d| ((**d as i64) - (h as i64)).abs())
            .unwrap(),
        None => *cands.iter().min_by_key(|d| d.abs()).unwrap(),
    };

    // 逐行复核：粗筛的 32 行可能有幸存者偏差
    if passes(prev, cur, lo, hi, pick) {
        return Some(pick);
    }
    for cand in cands.iter().copied().filter(|d| *d != pick) {
        if passes(prev, cur, lo, hi, cand) {
            return Some(cand);
        }
    }
    None
}

fn passes(prev: &Frame, cur: &Frame, lo: u32, hi: u32, shift: i32) -> bool {
    let d = shift.unsigned_abs();
    if d >= hi.saturating_sub(lo) {
        return false;
    }
    let end = hi - d;
    let (matched, total) = verify(prev, cur, lo, end, shift);
    total >= MIN_ROWS && matched as f32 / (total as f32) >= MATCH_RATIO
}

/// 抽样打分：返回"有内容的行里对上的比例"
fn probe(prev: &Frame, cur: &Frame, lo: u32, end: u32, shift: i32) -> f32 {
    let d = shift.unsigned_abs();
    let span = end - lo;
    let step = (span / PROBE_ROWS).max(1);
    let mut hit = 0;
    let mut total = 0;
    let mut y = lo;
    while y < end {
        let (a, b) = pair(y, d, shift);
        if !(cur.plain[a] || prev.plain[b]) {
            total += 1;
            if cur.sig[a] == prev.sig[b] {
                hit += 1;
            }
        }
        y += step;
    }
    if total == 0 {
        0.0
    } else {
        hit as f32 / total as f32
    }
}

/// 逐行复核，返回 (对上的行数, 有效行数)
fn verify(prev: &Frame, cur: &Frame, lo: u32, end: u32, shift: i32) -> (u32, u32) {
    let d = shift.unsigned_abs();
    let mut hit = 0;
    let mut total = 0;
    for y in lo..end {
        let (a, b) = pair(y, d, shift);
        if cur.plain[a] || prev.plain[b] {
            continue;
        }
        total += 1;
        if cur.sig[a] == prev.sig[b] {
            hit += 1;
        }
    }
    (hit, total)
}

/// 给定内容带里的位置 y 和位移，返回要在 cur / prev 里比的那两行。
/// 往下滚是 `(y, y+d)`，往上滚是 `(y+d, y)`。
#[inline]
fn pair(y: u32, d: u32, shift: i32) -> (usize, usize) {
    if shift > 0 {
        (y as usize, (y + d) as usize)
    } else {
        ((y + d) as usize, y as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一张"长网页"：每行都有自己的花纹，且不是纯色，
    /// 这样行指纹有区分度，能真实反映对齐算法。
    fn page(w: u32, h: u32) -> RgbaImage {
        let mut img = RgbaImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let r = ((x * 3 + y * 5) % 200 + 40) as u8;
                let g = ((x * 7 + y * 11) % 180 + 50) as u8;
                let b = ((x * 13 + y * 17) % 160 + 60) as u8;
                img.put_pixel(x, y, image::Rgba([r, g, b, 255]));
            }
        }
        img
    }

    fn window_img(img: &RgbaImage, y: u32, h: u32) -> RgbaImage {
        let (w, ih) = img.dimensions();
        let mut out = RgbaImage::new(w, h);
        for dy in 0..h {
            let sy = y + dy;
            for x in 0..w {
                let p = if sy < ih {
                    *img.get_pixel(x, sy)
                } else {
                    image::Rgba([0, 0, 0, 255])
                };
                out.put_pixel(x, dy, p);
            }
        }
        out
    }

    fn window(img: &RgbaImage, y: u32, h: u32) -> Frame {
        Frame::new(window_img(img, y, h))
    }

    /// 把帧的顶部 n 行改成"吸顶表头"的样子（每帧都一样）
    fn with_header(img: RgbaImage, n: u32) -> RgbaImage {
        let (w, h) = img.dimensions();
        let mut out = img;
        for y in 0..n.min(h) {
            for x in 0..w {
                let v = if (x / 8) % 2 == 0 { 30 } else { 200 };
                out.put_pixel(x, y, image::Rgba([v, v, v, 255]));
            }
        }
        out
    }

    #[test]
    fn stitches_plain_scroll() {
        let (w, h) = (240, 1200);
        let img = page(w, h);
        let fh = 400;
        let mut s = Stitcher::new(w, fh, 20000);

        for y in [0u32, 300, 600, 800] {
            let r = s.push(window(&img, y, fh));
            assert!(!matches!(r, Push::Rejected), "y={y} 被拒了: {r:?}");
        }

        let out = s.finish().expect("应该有图");
        assert_eq!(out.dimensions(), (w, h));
        assert!(out.as_raw() == img.as_raw(), "拼出来应该和原图逐像素一致");
    }

    #[test]
    fn ignores_sticky_header() {
        let (w, h) = (240, 1200);
        let base = page(w, h);
        let fh = 400;
        let hdr = 40;
        let mut s = Stitcher::new(w, fh, 20000);

        for y in [0u32, 300, 600, 800] {
            let f = Frame::new(with_header(window_img(&base, y, fh), hdr));
            let r = s.push(f);
            assert!(!matches!(r, Push::Rejected), "y={y} 被拒了: {r:?}");
        }

        let out = s.finish().expect("应该有图");
        // 表头只保留最初那一份，下面的内容不能重复
        assert_eq!(out.height(), hdr + (h - hdr), "高度应该正好是表头 + 剩余内容");
        for y in hdr..h {
            let want = base.get_pixel(w / 2, y);
            let got = out.get_pixel(w / 2, y);
            assert!(want == got, "第 {y} 行内容不对");
        }
    }

    #[test]
    fn same_frame_is_noop() {
        let img = page(200, 900);
        let mut s = Stitcher::new(200, 400, 20000);
        assert!(matches!(s.push(window(&img, 0, 400)), Push::Appended(_)));
        let before = s.height();
        assert_eq!(s.push(window(&img, 0, 400)), Push::Same);
        assert_eq!(s.height(), before);
    }

    #[test]
    fn slow_scroll_accumulates() {
        let (w, h) = (200, 900);
        let img = page(w, h);
        let mut s = Stitcher::new(w, 400, 20000);
        for y in (0..=(h - 400)).step_by(5) {
            s.push(window(&img, y, 400));
        }
        let out = s.finish().expect("应该有图");
        assert_eq!(out.height(), h, "每步 5px 累积起来也应该是完整的一张");
        assert!(out.as_raw() == img.as_raw(), "内容和原图不一致");
    }

    /// 从页面中间开始，然后一路**往上**滚回顶部：新内容应该拼到开头
    #[test]
    fn stitches_upward() {
        let (w, h) = (240, 1200);
        let img = page(w, h);
        let fh = 400;
        let mut s = Stitcher::new(w, fh, 20000);

        for y in [800u32, 500, 200, 0] {
            let r = s.push(window(&img, y, fh));
            assert!(!matches!(r, Push::Rejected), "y={y} 被拒了: {r:?}");
        }

        let out = s.finish().expect("应该有图");
        assert_eq!(out.dimensions(), (w, h), "往上滚也应该拼满整页");
        assert!(out.as_raw() == img.as_raw(), "往上滚拼出来的内容要一模一样");
    }

    /// 先往下、再往回、再往下：不能重复也不能丢内容
    #[test]
    fn handles_back_and_forth() {
        let (w, h) = (200, 1200);
        let img = page(w, h);
        let fh = 400;
        let mut s = Stitcher::new(w, fh, 20000);
        for y in [0u32, 300, 600, 400, 700, 800] {
            let r = s.push(window(&img, y, fh));
            assert!(!matches!(r, Push::Rejected), "y={y} 被拒了: {r:?}");
        }
        let out = s.finish().expect("应该有图");
        assert_eq!(out.dimensions(), (w, h), "来回滚也不该把图拼长或拼短");
        assert!(out.as_raw() == img.as_raw(), "内容和原图不一致");
    }

    #[test]
    fn stops_at_max_height() {
        let (w, h) = (200, 3000);
        let img = page(w, h);
        let mut s = Stitcher::new(w, 400, 1000);
        let mut full = false;
        for y in (0..=(h - 400)).step_by(300) {
            if s.push(window(&img, y, 400)) == Push::Full {
                full = true;
                break;
            }
        }
        assert!(full, "应该在上限处返回 Full");
        assert!(s.height() <= 1000);
    }

    #[test]
    fn rejects_unrelated_frame() {
        let a = page(200, 900);
        let b = page(200, 900);
        let mut s = Stitcher::new(200, 400, 20000);
        s.push(window(&a, 0, 400));
        // 完全另一张图（用反色模拟"切走了别的窗口"）
        let mut other = b;
        for p in other.pixels_mut() {
            p.0 = [255 - p.0[0], 255 - p.0[1], 255 - p.0[2], 255];
        }
        assert_eq!(s.push(window(&other, 0, 400)), Push::Rejected);
    }

    #[test]
    fn periodic_content_uses_hint() {
        // 每 100 行重复一次的页面：160 行重合时会出现多个同样好的位移
        let (w, h) = (200, 2000);
        let mut img = RgbaImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let band = y % 100;
                let r = ((x * 5 + band * 3) % 200 + 40) as u8;
                let g = ((x * 9 + band * 7) % 180 + 50) as u8;
                img.put_pixel(x, y, image::Rgba([r, g, 90, 255]));
            }
        }
        let mut s = Stitcher::new(w, 400, 20000);
        for y in [0u32, 150, 300, 450] {
            let r = s.push(window(&img, y, 400));
            assert!(!matches!(r, Push::Rejected), "y={y} 被拒了: {r:?}");
        }
        // 只要没崩、没有把图撑成两倍，就算过关（重复内容本身无法完美还原）
        let out = s.finish().expect("应该有图");
        assert!(out.height() >= 400, "至少要有第一帧");
    }

    #[test]
    fn sample_fills_whole_preview() {
        let img = page(120, 600);
        let mut s = Stitcher::new(120, 200, 20000);
        s.push(window(&img, 0, 200));
        s.push(window(&img, 100, 200));
        let (pw, ph) = (60u32, 100u32);
        let mut buf = vec![0u8; (pw * ph * 4) as usize];
        s.sample_bgra(&mut buf, pw, ph);
        // 每一行都得被写过（alpha 都是 255，不能留有 0）
        assert!(buf.chunks_exact(4).all(|p| p[3] == 255), "采样有漏");
        // 抽一个点核对通道顺序：BGRA
        // 画布 300 行缩到 100 行，py=10 对应画布第 30 行
        let px = *img.get_pixel(60, 30);
        let i = (10 * pw + 30) as usize * 4;
        assert_eq!(buf[i], px.0[2], "B 通道不对");
        assert_eq!(buf[i + 1], px.0[1], "G 通道不对");
        assert_eq!(buf[i + 2], px.0[0], "R 通道不对");
    }
}
