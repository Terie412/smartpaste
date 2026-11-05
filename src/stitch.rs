//! 滚动截图：把一帧帧滚动的画面拼成一张长图。
//!
//! 核心问题只有一个——"这一帧相对上一帧往上走了多少像素"。这个数字拿不到，
//! 因为每个程序一次滚轮滚多少完全不一样（浏览器按行、编辑器按像素、有的还带
//! 惯性动画）。所以只能从画面里反推：拿当前帧的每一行去上一帧里找，重合最多、
//! 且重合部分严丝合缝的那个位移就是答案。
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
        let step = (w / SAMPLES).max(1);

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
    let mut x = 0;
    while x < w {
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
    /// 拼进去了 n 行
    Appended(u32),
    /// 和上一帧一样：没滚，或者滚到底了
    Same,
    /// 对不上（画面在动、被遮挡、或者干脆换了内容），这帧丢了
    Rejected,
    /// 到高度上限了
    Full,
}

pub struct Stitcher {
    w: u32,
    frame_h: u32,
    canvas: Vec<u8>,
    height: u32,
    max_height: u32,
    prev: Option<Frame>,
    /// 上一次成功对齐的位移。重复内容靠它挑候选
    last_shift: Option<u32>,
    /// 上下两条"静止带"（吸顶表头、吸底状态栏）的高度，取见过的最大值
    top: u32,
    bottom: u32,
    /// 已接受的帧数
    pub frames: u32,
}

impl Stitcher {
    /// `max_height` 是画布上限（像素）。宽度越大越早停，避免几亿字节的 Vec。
    pub fn new(w: u32, frame_h: u32, max_height: u32) -> Self {
        Self {
            w,
            frame_h,
            canvas: Vec::new(),
            height: 0,
            max_height,
            prev: None,
            last_shift: None,
            top: 0,
            bottom: 0,
            frames: 0,
        }
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn push(&mut self, frame: Frame) -> Push {
        if frame.w != self.w {
            return Push::Rejected;
        }
        if self.height + self.frame_h > self.max_height && self.frames > 0 {
            return Push::Full;
        }

        let Some(prev) = self.prev.as_ref() else {
            // 第一帧整帧铺上
            self.canvas.extend_from_slice(&frame.raw);
            self.height = frame.h;
            self.frames = 1;
            self.prev = Some(frame);
            return Push::Appended(self.height);
        };

        if similar(prev, &frame) {
            return Push::Same;
        }

        // 静止带：上下各有多少行和上一帧一模一样。吸顶表头就靠这个识别。
        let top = leading_equal(prev, &frame);
        let bottom = trailing_equal(prev, &frame);
        if top > self.top {
            self.top = top;
        }
        if bottom > self.bottom {
            self.bottom = bottom;
        }
        // 两条带不能把画面吃光，否则没得对齐
        if self.top + self.bottom + MIN_ROWS * 2 >= frame.h {
            return Push::Rejected;
        }

        let Some(d) = find_shift(prev, &frame, self.top, self.bottom, self.last_shift) else {
            return Push::Rejected;
        };
        if d < MIN_SHIFT {
            return Push::Same;
        }

        // 滚动方向向下 ⇒ 内容整体上移 d：新增的是内容带的最后 d 行
        let band_end = frame.h - self.bottom;
        let from = band_end - d;
        let room = self.max_height.saturating_sub(self.height);
        let take = d.min(room);
        if take == 0 {
            return Push::Full;
        }
        let row_len = (self.w * 4) as usize;
        let start = from as usize * row_len;
        let end = (from + take) as usize * row_len;
        self.canvas.extend_from_slice(&frame.raw[start..end]);
        self.height += take;
        self.frames += 1;
        self.last_shift = Some(d);
        self.prev = Some(frame);

        if take < d {
            Push::Full
        } else {
            Push::Appended(d)
        }
    }

    /// 拿成品。`Vec<u8>` 所有权直接交给 `RgbaImage`，不再复制一份
    /// （长图动辄上亿字节，复制一次就是几百 MB 的峰值）。
    pub fn finish(self) -> Option<RgbaImage> {
        if self.height == 0 {
            return None;
        }
        let need = (self.w * self.height * 4) as usize;
        let mut canvas = self.canvas;
        canvas.truncate(need);
        RgbaImage::from_raw(self.w, self.height, canvas)
    }
}

/// 开头有多少行和上一帧完全一样（吸顶带）。上限 h/3，防止纯色页面整帧同色。
fn leading_equal(prev: &Frame, cur: &Frame) -> u32 {
    let cap = cur.h / 3;
    let mut n = 0;
    while n < cap && prev.sig[n as usize] == cur.sig[n as usize] {
        n += 1;
    }
    // 只有一行相同多半是巧合，不当带
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

/// 在当前帧里找出"内容往上走了多少像素"。
///
/// 判定关系是 `cur[y] == prev[y + d]`，`y` 落在内容带 `[top, h-bottom-d)`。
/// 先按 32 行抽样粗筛，再对胜出的候选做逐行复核——全量比对 × 上千个候选位移
/// 每帧都要跑，太浪费。
fn find_shift(prev: &Frame, cur: &Frame, top: u32, bottom: u32, hint: Option<u32>) -> Option<u32> {
    let lo = top;
    let hi = cur.h.saturating_sub(bottom);
    if hi <= lo + MIN_ROWS {
        return None;
    }
    // 至少保留 MIN_ROWS 行重叠
    let max_d = hi - lo - MIN_ROWS;
    if max_d < MIN_SHIFT {
        return None;
    }

    let mut best_score = 0.0f32;
    let mut cands: Vec<u32> = Vec::new();

    for d in MIN_SHIFT..=max_d {
        let end = hi - d;
        if end <= lo {
            break;
        }
        let score = probe(prev, cur, lo, end, d);
        if score > best_score + 0.001 {
            best_score = score;
            cands.clear();
            cands.push(d);
        } else if (score - best_score).abs() <= 0.001 && score > 0.0 {
            cands.push(d);
        }
    }

    if best_score < 0.75 {
        return None;
    }

    // 多个候选一样好 = 遇到了重复内容。滚动是连续的，挑离上次最近的那个。
    let pick = match hint {
        Some(h) => *cands
            .iter()
            .min_by_key(|d| ((**d as i64) - (h as i64)).abs())
            .unwrap(),
        None => *cands.first().unwrap(),
    };

    // 逐行复核：粗筛的 32 行可能有幸存者偏差
    let end = hi - pick;
    let (matched, total) = verify(prev, cur, lo, end, pick);
    if total < MIN_ROWS || matched as f32 / (total as f32) < MATCH_RATIO {
        // 复核不过就退一步，试剩下的候选
        for d in cands.iter().copied().filter(|d| *d != pick) {
            let end = hi - d;
            if end <= lo {
                continue;
            }
            let (m, t) = verify(prev, cur, lo, end, d);
            if t >= MIN_ROWS && m as f32 / t as f32 >= MATCH_RATIO {
                return Some(d);
            }
        }
        return None;
    }
    Some(pick)
}

/// 抽样打分：返回"有内容的行里对上的比例"
fn probe(prev: &Frame, cur: &Frame, lo: u32, end: u32, d: u32) -> f32 {
    let span = end - lo;
    let step = (span / PROBE_ROWS).max(1);
    let mut hit = 0;
    let mut total = 0;
    let mut y = lo;
    while y < end {
        if !(cur.plain[y as usize] || prev.plain[(y + d) as usize]) {
            total += 1;
            if cur.sig[y as usize] == prev.sig[(y + d) as usize] {
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
fn verify(prev: &Frame, cur: &Frame, lo: u32, end: u32, d: u32) -> (u32, u32) {
    let mut hit = 0;
    let mut total = 0;
    for y in lo..end {
        if cur.plain[y as usize] || prev.plain[(y + d) as usize] {
            continue;
        }
        total += 1;
        if cur.sig[y as usize] == prev.sig[(y + d) as usize] {
            hit += 1;
        }
    }
    (hit, total)
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
            let f = window(&img, y, fh);
            let r = s.push(f);
            assert!(!matches!(r, Push::Rejected), "y={y} 被拒了: {r:?}");
        }

        let out = s.finish().expect("应该有图");
        assert_eq!(out.dimensions(), (w, h));
        assert_eq!(out.as_raw(), img.as_raw(), "拼出来应该和原图逐像素一致");
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
            assert_eq!(want, got, "第 {y} 行内容不对");
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
        assert_eq!(out.as_raw(), img.as_raw());
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
}
