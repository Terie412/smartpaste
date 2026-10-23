//! Win32 小工具：宽字符、矩形、字体。

use windows_sys::Win32::Foundation::{HINSTANCE, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    CreateFontW, DeleteObject, ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
    DEFAULT_PITCH, FF_DONTCARE, FW_NORMAL, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;

pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn hinstance() -> HINSTANCE {
    unsafe { GetModuleHandleW(std::ptr::null()) }
}

pub const fn rect(l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT {
        left: l,
        top: t,
        right: r,
        bottom: b,
    }
}

pub fn pt_in(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// 创建 UI 字体（微软雅黑，常规字重）。调用方负责 DeleteObject。
pub fn ui_font(px: i32) -> HFONT {
    ui_font_w(px, FW_NORMAL as i32)
}

/// 创建 UI 字体，可指定字重。调用方负责 DeleteObject。
pub fn ui_font_w(px: i32, weight: i32) -> HFONT {
    ui_font_ex(px, weight, "Microsoft YaHei")
}

/// 指定字号、字重、字体族。拉丁字母用 Segoe UI 比雅黑更挺，
/// 中文交给系统的字体链接回退。
pub fn ui_font_ex(px: i32, weight: i32, face: &str) -> HFONT {
    unsafe {
        let face = to_wide(face);
        CreateFontW(
            -px,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            // 用灰度抗锯齿，不用 ClearType：这些字画在深色标签和半透明蒙层上，
            // 子像素渲染会在白字边上泛出橙蓝色
            ANTIALIASED_QUALITY as u32,
            (DEFAULT_PITCH | FF_DONTCARE) as u32,
            face.as_ptr(),
        )
    }
}

pub fn delete_font(f: HFONT) {
    if !f.is_null() {
        unsafe {
            DeleteObject(f as HGDIOBJ);
        }
    }
}

/// 轻量诊断日志，落到 %TEMP%\smartpaste.log。
pub fn trace(msg: &str) {
    use std::io::Write;
    let path = std::env::temp_dir().join("smartpaste.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{msg}");
    }
}
