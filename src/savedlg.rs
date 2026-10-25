//! 「另存为」对话框。
//!
//! 用系统自带的 `GetSaveFileNameW`：不用自己画界面，覆盖确认、新建文件夹、
//! 记忆路径这些都是现成的。每次保存都弹窗，由用户决定存哪儿——
//! 只有"上次存过的目录"会被记下来，下次直接打开那里。

use std::path::PathBuf;

use windows_sys::Win32::Foundation::{HWND, SYSTEMTIME};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::UI::Controls::Dialogs::{
    GetSaveFileNameW, OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};

/// 默认文件名：SmartPaste_20261022-175509.png
pub fn default_name() -> String {
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    format!(
        "SmartPaste_{:04}{:02}{:02}-{:02}{:02}{:02}.png",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}

/// 弹框让用户选路径。返回 None 表示用户取消。
pub fn ask(owner: HWND, initial_dir: Option<&str>, default_name: &str) -> Option<PathBuf> {
    ask_file(
        owner,
        initial_dir,
        default_name,
        "PNG 图片\0*.png\0所有文件\0*.*\0\0",
        "保存截图",
        "png",
    )
}

/// 录像的另存为。
pub fn ask_video(owner: HWND, initial_dir: Option<&str>, default_name: &str) -> Option<PathBuf> {
    ask_file(
        owner,
        initial_dir,
        default_name,
        "MP4 视频\0*.mp4\0所有文件\0*.*\0\0",
        "保存录像",
        "mp4",
    )
}

/// 录像默认文件名：SmartPaste_20261025-231500.mp4
pub fn video_name() -> String {
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    format!(
        "SmartPaste_{:04}{:02}{:02}-{:02}{:02}{:02}.mp4",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}

fn ask_file(
    owner: HWND,
    initial_dir: Option<&str>,
    default_name: &str,
    filter_str: &str,
    title_str: &str,
    ext_str: &str,
) -> Option<PathBuf> {
    let filter: Vec<u16> = filter_str.encode_utf16().collect();
    let mut file: Vec<u16> = vec![0; 1024];
    let name: Vec<u16> = default_name.encode_utf16().collect();
    if name.len() < file.len() {
        file[..name.len()].copy_from_slice(&name);
    }
    let dir: Vec<u16> = match initial_dir {
        Some(d) => d.encode_utf16().chain([0]).collect(),
        None => vec![0],
    };
    let title: Vec<u16> = title_str.encode_utf16().chain([0]).collect();
    let ext: Vec<u16> = ext_str.encode_utf16().chain([0]).collect();

    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = owner;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.nFilterIndex = 1;
    ofn.lpstrFile = file.as_mut_ptr();
    ofn.nMaxFile = file.len() as u32;
    ofn.lpstrInitialDir = dir.as_ptr();
    ofn.lpstrTitle = title.as_ptr();
    ofn.lpstrDefExt = ext.as_ptr();
    // NOCHANGEDIR：别把进程的工作目录改掉，否则后续相对路径全乱
    ofn.Flags = OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;

    let ok = unsafe { GetSaveFileNameW(&mut ofn) };
    if ok == 0 {
        return None;
    }
    let len = file.iter().position(|&c| c == 0).unwrap_or(0);
    if len == 0 {
        return None;
    }
    Some(PathBuf::from(String::from_utf16_lossy(&file[..len])))
}
