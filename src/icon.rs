//! 内置图标。
//!
//! ICO 用 include_bytes! 塞进二进制，运行时从内存构造 HICON —
//! 单文件分发，不需要旁边跟着一个 .ico。
//!
//! 按目标尺寸挑帧：托盘用 16/20/24（这些帧是简化过的版本），
//! 大尺寸用完整爪形。

use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconFromResourceEx, DestroyIcon, HICON, LR_DEFAULTCOLOR,
};

const ICO: &[u8] = include_bytes!("../assets/smartpaste.ico");

fn le16(at: usize) -> u16 {
    u16::from_le_bytes([ICO[at], ICO[at + 1]])
}

fn le32(at: usize) -> u32 {
    u32::from_le_bytes([ICO[at], ICO[at + 1], ICO[at + 2], ICO[at + 3]])
}

/// 找最接近 px 的一帧，返回 (偏移, 长度)。
/// 优先取「不小于 px 的最小帧」，没有就退到最大的那帧。
fn frame_for(px: i32) -> Option<(usize, usize)> {
    if ICO.len() < 6 || le16(0) != 0 || le16(2) != 1 {
        return None;
    }
    let count = le16(4) as usize;
    let mut best: Option<(usize, usize, i32)> = None;

    for i in 0..count {
        let entry = 6 + i * 16;
        if entry + 16 > ICO.len() {
            break;
        }
        // 宽高字段为 0 表示 256
        let w = if ICO[entry] == 0 { 256 } else { ICO[entry] as i32 };
        let len = le32(entry + 8) as usize;
        let off = le32(entry + 12) as usize;
        if len == 0 || off + len > ICO.len() {
            continue;
        }

        let take = match best {
            None => true,
            Some((_, _, cur)) => {
                if w >= px {
                    // 都够大时取更小的；当前不够大则直接替换
                    cur < px || w < cur
                } else {
                    // 都不够大时取更大的
                    w > cur
                }
            }
        };
        if take {
            best = Some((off, len, w));
        }
    }

    best.map(|(off, len, _)| (off, len))
}

/// 构造一个 HICON。调用方负责 `destroy`。
pub fn for_size(px: i32) -> HICON {
    let Some((off, len)) = frame_for(px) else {
        return std::ptr::null_mut();
    };
    unsafe {
        CreateIconFromResourceEx(
            ICO.as_ptr().add(off),
            len as u32,
            1, // fIcon
            0x0003_0000,
            0, // 用帧自带尺寸
            0,
            LR_DEFAULTCOLOR,
        )
    }
}

pub fn destroy(icon: HICON) {
    if !icon.is_null() {
        unsafe { DestroyIcon(icon) };
    }
}
