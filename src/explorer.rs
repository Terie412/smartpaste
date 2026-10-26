//! 资源管理器相关：选文件夹、打开目录。
//!
//! `windows-sys` 0.59 没有导出 `SHBrowseForFolderW`，这里改用 `windows` crate。

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, MAX_PATH};
use windows::Win32::UI::Shell::{
    BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW, ILFree, SHBrowseForFolderW,
    SHGetPathFromIDListW, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// 弹系统文件夹选择框。`owner` 是原始 HWND 指针（windows-sys 和 windows 的句柄类型不同）。
pub fn pick_dir(owner: *mut std::ffi::c_void) -> Option<String> {
    unsafe {
        let title = HSTRING::from("选择录像保存目录");
        let bi = BROWSEINFOW {
            hwndOwner: HWND(owner),
            lpszTitle: PCWSTR(title.as_ptr()),
            ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE,
            ..Default::default()
        };

        let pidl = SHBrowseForFolderW(&bi);
        if pidl.is_null() {
            return None;
        }

        let mut buf = [0u16; MAX_PATH as usize];
        let ok = SHGetPathFromIDListW(pidl, &mut buf);
        ILFree(Some(pidl as *const _));
        if !ok.as_bool() {
            return None;
        }

        let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}

/// 用资源管理器打开目录。
pub fn open_dir(path: &str) {
    unsafe {
        let op = HSTRING::from("open");
        let p = HSTRING::from(path);
        ShellExecuteW(
            None,
            PCWSTR(op.as_ptr()),
            PCWSTR(p.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// 打开文件所在目录并选中它。`ShellExecuteW` 没有"选中"这个动作，
/// 得靠 `explorer.exe /select,` 这条老规矩。
pub fn reveal(path: &str) {
    if std::process::Command::new("explorer.exe")
        .arg(format!("/select,{path}"))
        .spawn()
        .is_ok()
    {
        return;
    }
    // 文件路径异常时至少把目录打开
    if let Some(dir) = std::path::Path::new(path).parent() {
        open_dir(&dir.display().to_string());
    }
}
