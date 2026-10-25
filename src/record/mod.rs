//! 录屏：DXGI Desktop Duplication → D3D11 画布 → Media Foundation 硬编 → MP4。
//!
//! 全通路保持在 GPU 上：整帧从不回读 CPU，只有光标覆盖的小块会往返一次。
//!
//! 画布分工：
//! - `clean` 保存最近一次 DDA 的原始画面，不带光标
//! - `outputs[n]` 是每帧要交给编码器的成品，轮转使用，避免编码器还在读就被覆盖

mod cursor;
mod dda;
mod encoder;
pub mod session;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::core::{Error, Result};
use windows::Win32::Foundation::{CloseHandle, E_FAIL, HANDLE};
use windows::Win32::Media::MediaFoundation::{MFShutdown, MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, SetWaitableTimer, WaitForSingleObject,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS,
};

pub use cursor::CursorBlender;
pub use dda::Capturer;
pub use encoder::Encoder;

/// 光标小块的最小边长；实际取系统光标尺寸。
const MIN_BLOCK: u32 = 64;

pub struct Report {
    pub frames: u64,
    pub dropped: u64,
    pub seconds: f64,
    pub hardware: bool,
    pub encoder: String,
    pub cpu_pct: f64,
    pub grab_ms: f64,
    pub write_ms: f64,
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Report {
    pub fn summary(&self) -> String {
        format!(
            "rectest: {}x{}@{} 写入 {} 帧 / 丢 {} 帧 / 成片 {:.2}s (实际 {:.2}s) | 编码器 {} ({}) | CPU {:.1}% 单核 | grab {:.2}ms write {:.2}ms | {}",
            self.width,
            self.height,
            self.fps,
            self.frames,
            self.dropped,
            self.frames as f64 / self.fps as f64,
            self.seconds,
            self.encoder,
            if self.hardware { "硬件" } else { "软件" },
            self.cpu_pct,
            self.grab_ms,
            self.write_ms,
            self.path
        )
    }
}

/// 一次录制的参数。
pub struct Options {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub path: String,
    /// 自测用：到点自动停。正常录制传 None，靠 `stop` 停。
    pub max_seconds: Option<f64>,
    pub stop: Arc<AtomicBool>,
    /// 已录秒数，UI 线程读它刷托盘
    pub progress: Arc<AtomicU64>,
}

/// 录制指定区域。`x, y, w, h` 是屏幕物理像素。
pub fn run(opts: Options) -> Result<Report> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
    }

    let result = run_inner(&opts);

    unsafe {
        let _ = MFShutdown();
        CoUninitialize();
    }
    result
}

fn run_inner(opts: &Options) -> Result<Report> {
    let (x, y, fps, path) = (opts.x, opts.y, opts.fps, opts.path.as_str());

    // H.264 要求偶数宽高；奇数时向内收一像素
    let w = opts.width & !1;
    let h = opts.height & !1;
    if w < 128 || h < 128 {
        return Err(Error::new(E_FAIL, "选区太小，最小 128x128"));
    }
    if w > 3840 || h > 2160 {
        return Err(Error::new(E_FAIL, "v1 最大 4K，请缩小选区"));
    }
    if fps != 30 && fps != 60 {
        return Err(Error::new(E_FAIL, "帧率只支持 30 / 60"));
    }

    let bitrate = ((w as f64) * (h as f64) * (fps as f64) * 0.08) as u32;
    let bitrate = bitrate.clamp(2_000_000, 50_000_000);

    let mut blender = CursorBlender::new().ok_or_else(|| Error::new(E_FAIL, "光标混合器初始化失败"))?;
    let block = (blender.size as u32).max(MIN_BLOCK);
    let mut cap = Capturer::new(x, y, w, h, block)?;
    let mut enc = Encoder::new(path, &cap.device, w, h, fps, bitrate)?;

    // 先拿到第一帧，否则画布是空的
    let prime_deadline = Instant::now() + Duration::from_secs(3);
    while !cap.primed {
        let _ = cap.grab(16)?;
        if Instant::now() > prime_deadline {
            return Err(Error::new(E_FAIL, "3 秒内没拿到第一帧，DDA 可能被占用"));
        }
    }

    let clock = Clock::new();
    let total = opts
        .max_seconds
        .map(|s| Duration::from_secs_f64(s.max(1.0)));
    let start = Instant::now();
    let cpu_start = cpu_100ns();

    let mut frame: u64 = 0;
    let mut written: u64 = 0;
    let mut dropped: u64 = 0;
    let mut grab_ms = 0.0f64;
    let mut write_ms = 0.0f64;
    let mut block_buf = vec![0u32; (block * block) as usize];

    loop {
        let now = start.elapsed();
        if let Some(t) = total {
            if now >= t {
                break;
            }
        }
        if opts.stop.load(Ordering::Relaxed) {
            break;
        }
        opts.progress.store(now.as_secs(), Ordering::Relaxed);

        // 编码跟不上时，跳帧追赶；时间戳照常推进，成片不会变慢
        let want = (now.as_secs_f64() * fps as f64).floor() as u64;
        if want > frame {
            dropped += want - frame;
            frame = want;
        }

        let target = Duration::from_nanos(frame * 1_000_000_000 / fps as u64);
        if target > start.elapsed() {
            clock.wait_until(start + target);
        }

        let slot = (written as usize) % dda::CANVAS_COUNT;

        let t0 = Instant::now();
        let _ = cap.grab(0)?;
        cap.copy_clean_to(slot);
        grab_ms += t0.elapsed().as_secs_f64() * 1000.0;

        if let Some(cs) = blender.query() {
            let lx = cs.x - x;
            let ly = cs.y - y;
            let bx = lx.max(0);
            let by = ly.max(0);
            let bw = ((lx + blender.size).min(w as i32) - bx).max(0);
            let bh = ((ly + blender.size).min(h as i32) - by).max(0);
            if bw > 0 && bh > 0 {
                let n = (bw * bh) as usize;
                let buf = &mut block_buf[..n];
                if cap
                    .read_block(slot, bx as u32, by as u32, bw as u32, bh as u32, buf)
                    .is_ok()
                    && blender.blend(&cs, buf, bw, bh, x + bx, y + by)
                {
                    cap.write_block(slot, bx as u32, by as u32, bw as u32, bh as u32, buf);
                }
            }
        }

        let t1 = Instant::now();
        let ts = (frame as i64) * 10_000_000 / fps as i64;
        let dur = 10_000_000 / fps as i64;
        enc.write(cap.canvas(slot), ts, dur)?;
        write_ms += t1.elapsed().as_secs_f64() * 1000.0;

        frame += 1;
        written += 1;
    }

    let elapsed = start.elapsed().as_secs_f64();
    let cpu_end = cpu_100ns();

    if written == 0 {
        return Err(Error::new(E_FAIL, "没有录到任何帧"));
    }

    enc.finish()?;
    unsafe {
        let _ = MFShutdown();
    }

    let n = written.max(1) as f64;
    Ok(Report {
        frames: written,
        dropped,
        seconds: elapsed,
        hardware: enc.hardware,
        encoder: enc.encoder.clone(),
        cpu_pct: (cpu_end.saturating_sub(cpu_start) as f64) / (elapsed * 10_000_000.0) * 100.0,
        grab_ms: grab_ms / n,
        write_ms: write_ms / n,
        path: path.to_string(),
        width: w,
        height: h,
        fps,
    })
}

/// 固定时长的自测入口：录 `seconds` 秒就停，其余和正常录制同一条路径。
pub fn rectest(
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    seconds: f64,
    path: &str,
    fps: u32,
) -> Result<Report> {
    run(Options {
        x,
        y,
        width: w,
        height: h,
        fps,
        path: path.to_string(),
        max_seconds: Some(seconds),
        stop: Arc::new(AtomicBool::new(false)),
        progress: Arc::new(AtomicU64::new(0)),
    })
}

/// 高精度等待定时器。`Sleep(1)` 的精度是 15.6ms，60fps 没法用。
struct Clock(Option<HANDLE>);

impl Clock {
    fn new() -> Self {
        unsafe {
            let h = CreateWaitableTimerExW(
                None,
                windows::core::PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
            .ok();
            Self(h)
        }
    }

    fn wait_until(&self, deadline: Instant) {
        let now = Instant::now();
        if deadline <= now {
            return;
        }
        let Some(h) = self.0 else {
            std::thread::sleep(deadline - now);
            return;
        };
        // SetWaitableTimer 的相对时间是 100ns，负值表示"从调用时刻起"
        let due = -(((deadline - now).as_nanos() as i64 / 100).max(1));
        unsafe {
            if SetWaitableTimer(h, &due, 0, None, None, false).is_ok() {
                WaitForSingleObject(h, u32::MAX);
            }
        }
    }
}

impl Drop for Clock {
    fn drop(&mut self) {
        if let Some(h) = self.0 {
            unsafe {
                let _ = CloseHandle(h);
            }
        }
    }
}

/// 当前进程累计 CPU 时间，单位 100ns。
fn cpu_100ns() -> u64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    unsafe {
        let mut create: FILETIME = std::mem::zeroed();
        let mut exit: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        if GetProcessTimes(GetCurrentProcess(), &mut create, &mut exit, &mut kernel, &mut user) == 0 {
            return 0;
        }
        let pack = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
        pack(kernel) + pack(user)
    }
}
