//! UI Automation 命中测试：算出鼠标下面"想截的那块区域"。
//!
//! 一定要在独立线程里跑：UIA 调用的是别的进程的 provider，某些应用
//! （Electron、自绘 UI）会阻塞几百毫秒甚至不返回，放在 UI 线程里
//! 会让整个遮罩卡住。
//!
//! 主线程只做两件事：`request(x, y)` 丢一个坐标过去，`take(seq)` 取结果。
//! 中间靠一个序号对齐，避免拿到过期结果。
//!
//! ## 现状（未启用，overlay.rs 里 PREDICT_ON = false）
//!
//! 遮罩是个全屏置顶窗口，UIA 会给**每个可见窗口**合成一个元素，
//! 所以 `ElementFromPoint` 永远返回遮罩自己（实测 hwnd 就是遮罩）。
//! 试过两条"标准做法"，都不行：
//!   - WM_GETOBJECT 返回 0（无 provider）：UIA 照样合成窗口元素
//!   - WS_EX_TRANSPARENT：只影响鼠标命中，不影响 UIA
//!
//! 现在走的是"按 z 序找遮罩下面那层窗口，再在它的子树里逐层下钻"，
//! 配合查询前把遮罩藏起来（几毫秒）。功能能跑通，但在这台机器上不稳定：
//! 桌面上有常驻的透明全屏层（UnityWndClass），z 序里排在真正目标窗口前面，
//! 会被当成"整屏"选中；过滤规则一收紧又会误杀最大化窗口。
//!
//! 下一步要么接受"藏遮罩查询"（会闪、会丢焦点），要么给遮罩写一个
//! 报告空矩形的 UIA provider（工作量大但干净）。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Foundation::{POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
use windows_sys::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows_sys::Win32::UI::Accessibility::CUIAutomation;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    ChildWindowFromPointEx, GetClassNameW, GetWindow, GetWindowLongW, GetWindowRect,
    IsWindowVisible, WindowFromPoint, CWP_SKIPINVISIBLE, CWP_SKIPTRANSPARENT, GW_HWNDNEXT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GWL_EXSTYLE, WS_EX_TRANSPARENT};

const IID_IUIAUTOMATION: windows_sys::core::GUID =
    windows_sys::core::GUID::from_u128(0x30cbe57d_d9d0_452a_ab13_7ac5ac4825ee);

// vtable 下标，照 UIAutomationClient.h 里各 Vtbl 的声明顺序数出来的。
// 记错了不会编译失败，只会在运行时调错函数——所以别随手改。
const VT_ELEMENT_FROM_POINT: usize = 7; // IUIAutomation
const VT_CONTROL_VIEW_WALKER: usize = 14; // IUIAutomation
const VT_RELEASE: usize = 2; // IUnknown
const EL_BOUNDING_RECT: usize = 43; // IUIAutomationElement
const EL_OFFSCREEN: usize = 38; // IUIAutomationElement
const EL_WINDOW_HANDLE: usize = 36; // IUIAutomationElement
const TW_PARENT: usize = 3; // IUIAutomationTreeWalker
const VT_ELEMENT_FROM_HANDLE: usize = 6; // IUIAutomation
const VT_CREATE_TRUE_CONDITION: usize = 21; // IUIAutomation
const EL_FIND_ALL: usize = 6; // IUIAutomationElement
const ARR_LEN: usize = 3; // IUIAutomationElementArray
const ARR_ITEM: usize = 4; // IUIAutomationElementArray
const SCOPE_CHILDREN: i32 = 2; // TreeScope_Children

static TX: OnceLock<mpsc::Sender<(u32, i32, i32)>> = OnceLock::new();
static RESULT: Mutex<Option<(u32, Option<(RECT, usize)>)>> = Mutex::new(None);
static SEQ: AtomicU32 = AtomicU32::new(0);

/// 起后台线程。重复调用无副作用。
pub fn start(skip_class: &'static str) {
    let (tx, rx) = mpsc::channel::<(u32, i32, i32)>();
    if TX.set(tx).is_err() {
        return;
    }

    thread::spawn(move || unsafe {
        if CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) < 0 {
            return;
        }
        let mut uia: *mut core::ffi::c_void = std::ptr::null_mut();
        let hr = CoCreateInstance(
            &CUIAutomation,
            std::ptr::null_mut(),
            CLSCTX_INPROC_SERVER,
            &IID_IUIAUTOMATION,
            &mut uia,
        );
        if hr < 0 || uia.is_null() {
            CoUninitialize();
            return;
        }

        let mut walker: *mut core::ffi::c_void = std::ptr::null_mut();
        let get_walker: unsafe extern "system" fn(
            *mut core::ffi::c_void,
            *mut *mut core::ffi::c_void,
        ) -> i32 = std::mem::transmute(*vtbl(uia).add(VT_CONTROL_VIEW_WALKER));
        get_walker(uia, &mut walker);

        while let Ok(mut req) = rx.recv() {
            // 排队期间鼠标又动了：只算最新的那个，别一个个补
            while let Ok(newer) = rx.try_recv() {
                req = newer;
            }
            let rect = hit(uia, walker, skip_class, req.1, req.2);
            if let Ok(mut slot) = RESULT.lock() {
                *slot = Some((req.0, rect));
            }
        }

        if !walker.is_null() {
            release(walker);
        }
        release(uia);
        CoUninitialize();
    });
}

/// 请求命中测试，返回本次的序号。主线程自己节流，别每帧都调。
pub fn request(x: i32, y: i32) -> u32 {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    if let Some(tx) = TX.get() {
        let _ = tx.send((seq, x, y));
    }
    seq
}

/// 取结果：只有序号对得上（不早于 seq）才返回，取完清空。
pub fn take(seq: u32) -> Option<Option<(RECT, usize)>> {
    let mut slot = RESULT.lock().ok()?;
    match *slot {
        Some((s, r)) if s >= seq => {
            *slot = None;
            Some(r)
        }
        _ => None,
    }
}

unsafe fn vtbl(p: *mut core::ffi::c_void) -> *const *const core::ffi::c_void {
    *(p as *const *const *const core::ffi::c_void)
}

unsafe fn release(p: *mut core::ffi::c_void) {
    let f: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32 =
        std::mem::transmute(*vtbl(p).add(VT_RELEASE));
    f(p);
}

unsafe fn element_from_point(
    uia: *mut core::ffi::c_void,
    x: i32,
    y: i32,
) -> *mut core::ffi::c_void {
    let f: unsafe extern "system" fn(
        *mut core::ffi::c_void,
        POINT,
        *mut *mut core::ffi::c_void,
    ) -> i32 = std::mem::transmute(*vtbl(uia).add(VT_ELEMENT_FROM_POINT));
    let mut el: *mut core::ffi::c_void = std::ptr::null_mut();
    if f(uia, POINT { x, y }, &mut el) < 0 {
        return std::ptr::null_mut();
    }
    el
}

unsafe fn rect_of(el: *mut core::ffi::c_void) -> RECT {
    let f: unsafe extern "system" fn(*mut core::ffi::c_void, *mut RECT) -> i32 =
        std::mem::transmute(*vtbl(el).add(EL_BOUNDING_RECT));
    let mut r: RECT = std::mem::zeroed();
    if f(el, &mut r) < 0 {
        return std::mem::zeroed();
    }
    r
}

unsafe fn is_offscreen(el: *mut core::ffi::c_void) -> bool {
    let f: unsafe extern "system" fn(*mut core::ffi::c_void, *mut i32) -> i32 =
        std::mem::transmute(*vtbl(el).add(EL_OFFSCREEN));
    let mut v: i32 = 0;
    f(el, &mut v) >= 0 && v != 0
}

unsafe fn parent_of(
    walker: *mut core::ffi::c_void,
    el: *mut core::ffi::c_void,
) -> *mut core::ffi::c_void {
    if walker.is_null() {
        return std::ptr::null_mut();
    }
    let f: unsafe extern "system" fn(
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        *mut *mut core::ffi::c_void,
    ) -> i32 = std::mem::transmute(*vtbl(walker).add(TW_PARENT));
    let mut p: *mut core::ffi::c_void = std::ptr::null_mut();
    if f(walker, el, &mut p) < 0 {
        return std::ptr::null_mut();
    }
    p
}

/// 命中点所在的元素，往上找到第一个"像样"的矩形。
unsafe fn window_of(el: *mut core::ffi::c_void) -> usize {
    let f: unsafe extern "system" fn(*mut core::ffi::c_void, *mut usize) -> i32 =
        std::mem::transmute(*vtbl(el).add(EL_WINDOW_HANDLE));
    let mut h: usize = 0;
    f(el, &mut h);
    h
}

unsafe fn element_from_handle(uia: *mut core::ffi::c_void, hwnd: isize) -> *mut core::ffi::c_void {
    let f: unsafe extern "system" fn(
        *mut core::ffi::c_void,
        isize,
        *mut *mut core::ffi::c_void,
    ) -> i32 = std::mem::transmute(*vtbl(uia).add(VT_ELEMENT_FROM_HANDLE));
    let mut el: *mut core::ffi::c_void = std::ptr::null_mut();
    if f(uia, hwnd, &mut el) < 0 {
        return std::ptr::null_mut();
    }
    el
}

unsafe fn find_children(
    el: *mut core::ffi::c_void,
    cond: *mut core::ffi::c_void,
) -> *mut core::ffi::c_void {
    let f: unsafe extern "system" fn(
        *mut core::ffi::c_void,
        i32,
        *mut core::ffi::c_void,
        *mut *mut core::ffi::c_void,
    ) -> i32 = std::mem::transmute(*vtbl(el).add(EL_FIND_ALL));
    let mut arr: *mut core::ffi::c_void = std::ptr::null_mut();
    if f(el, SCOPE_CHILDREN, cond, &mut arr) < 0 {
        return std::ptr::null_mut();
    }
    arr
}

unsafe fn array_len(arr: *mut core::ffi::c_void) -> i32 {
    let f: unsafe extern "system" fn(*mut core::ffi::c_void, *mut i32) -> i32 =
        std::mem::transmute(*vtbl(arr).add(ARR_LEN));
    let mut n = 0;
    if f(arr, &mut n) < 0 {
        return 0;
    }
    n
}

unsafe fn array_item(arr: *mut core::ffi::c_void, i: i32) -> *mut core::ffi::c_void {
    let f: unsafe extern "system" fn(
        *mut core::ffi::c_void,
        i32,
        *mut *mut core::ffi::c_void,
    ) -> i32 = std::mem::transmute(*vtbl(arr).add(ARR_ITEM));
    let mut el: *mut core::ffi::c_void = std::ptr::null_mut();
    if f(arr, i, &mut el) < 0 {
        return std::ptr::null_mut();
    }
    el
}

/// 在某个顶层窗口的子树里，逐层挑"包含这个点、面积最小"的子元素往下钻。
unsafe fn drill_down(
    uia: *mut core::ffi::c_void,
    hwnd: isize,
    x: i32,
    y: i32,
) -> (*mut core::ffi::c_void, bool) {
    let mut cur = element_from_handle(uia, hwnd);
    if cur.is_null() {
        return (cur, false);
    }
    let mut descended = false;

    let mk: unsafe extern "system" fn(*mut core::ffi::c_void, *mut *mut core::ffi::c_void) -> i32 =
        std::mem::transmute(*vtbl(uia).add(VT_CREATE_TRUE_CONDITION));
    let mut cond: *mut core::ffi::c_void = std::ptr::null_mut();
    mk(uia, &mut cond);
    if cond.is_null() {
        return (cur, descended);
    }

    for _ in 0..12 {
        let arr = find_children(cur, cond);
        if arr.is_null() {
            break;
        }
        let n = array_len(arr);
        if std::env::var_os("UIA_DEBUG").is_some() {
            eprintln!("  drill: {} children", n);
        }

        let mut next: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut best = i64::MAX;
        for i in 0..n {
            let child = array_item(arr, i);
            if child.is_null() {
                continue;
            }
            let r = rect_of(child);
            let inside = r.left <= x && x < r.right && r.top <= y && y < r.bottom;
            if inside && !is_offscreen(child) {
                let area = (r.right - r.left) as i64 * (r.bottom - r.top) as i64;
                if area > 0 && area < best {
                    best = area;
                    if !next.is_null() {
                        release(next);
                    }
                    next = child;
                    continue;
                }
            }
            release(child);
        }
        release(arr);

        if next.is_null() {
            break;
        }
        release(cur);
        cur = next;
        descended = true;
    }

    release(cond);
    (cur, descended)
}

/// 一路 ChildWindowFromPointEx 找到最深的那个子窗口。
///
/// Chrome 这类程序，控件全挂在子窗口（Chrome_RenderWidgetHostHWND）上，
/// 顶层窗口的 UIA 子树是空的，从顶层开始下钻只会拿到自己。
unsafe fn deepest_child(mut h: HWND, x: i32, y: i32) -> HWND {
    for _ in 0..8 {
        let mut pt = POINT { x, y };
        ScreenToClient(h, &mut pt);
        let c = ChildWindowFromPointEx(h, pt, CWP_SKIPINVISIBLE | CWP_SKIPTRANSPARENT);
        if c.is_null() || c == h {
            break;
        }
        h = c;
    }
    h
}

unsafe fn hit(
    uia: *mut core::ffi::c_void,
    walker: *mut core::ffi::c_void,
    skip_class: &str,
    x: i32,
    y: i32,
) -> Option<(RECT, usize)> {
    let screen_area = screen_area();

    let mut h = WindowFromPoint(POINT { x, y });
    let mut guard = 0;
    while !h.is_null() && guard < 16 {
        guard += 1;
        let mut buf = [0u16; 128];
        let n = GetClassNameW(h, buf.as_mut_ptr(), 128);
        let cls = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
        let mut wr: RECT = std::mem::zeroed();
        GetWindowRect(h, &mut wr);
        let inside = wr.left <= x && x < wr.right && wr.top <= y && y < wr.bottom;
        let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
        let click_through = ex & WS_EX_TRANSPARENT != 0;

        if cls != skip_class && inside && !click_through && IsWindowVisible(h) != 0 {
            let deep = deepest_child(h, x, y);
            let (el, descended) = drill_down(uia, deep as isize, x, y);
            if !el.is_null() {
                let r = rect_of(el);
                let area = (r.right - r.left) as i64 * (r.bottom - r.top) as i64;
                // 没能下钻、又占满整屏：多半是看不见的悬浮层，继续往下找
                let useless = !descended && area * 100 >= screen_area * 95;
                if !useless {
                    let win = window_of(el);
                    return finish(el, r, win);
                }
            }
        }
        h = GetWindow(h, GW_HWNDNEXT);
    }

    // 兜底：全局命中（没遮罩时才有用，遮罩下会返回遮罩自己）
    let mut cur = element_from_point(uia, x, y);
    if cur.is_null() {
        return None;
    }
    let mut best = None;
    for _ in 0..12 {
        let r = rect_of(cur);
        if !is_offscreen(cur) && r.right - r.left >= 8 && r.bottom - r.top >= 8 {
            best = Some((r, window_of(cur)));
            break;
        }
        let p = parent_of(walker, cur);
        if p.is_null() {
            break;
        }
        release(cur);
        cur = p;
    }
    release(cur);
    // 兜底路径也要拒绝"整屏"：预测一整屏没有意义，还不如让用户自己拖
    if let Some((r, _)) = best {
        let area = (r.right - r.left) as i64 * (r.bottom - r.top) as i64;
        if area * 100 >= screen_area * 99 {
            return None;
        }
    }
    best
}

/// 收尾：把元素往上爬，取出可用的矩形并释放引用
unsafe fn finish(el: *mut core::ffi::c_void, mut r: RECT, win: usize) -> Option<(RECT, usize)> {
    if r.right - r.left < 8 || r.bottom - r.top < 8 {
        r = std::mem::zeroed();
    }
    release(el);
    if r.right <= r.left {
        return None;
    }
    Some((r, win))
}

fn screen_area() -> i64 {
    unsafe {
        let w = windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows_sys::Win32::UI::WindowsAndMessaging::SM_CXVIRTUALSCREEN,
        ) as i64;
        let h = windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows_sys::Win32::UI::WindowsAndMessaging::SM_CYVIRTUALSCREEN,
        ) as i64;
        (w * h).max(1)
    }
}

/// 找到遮罩窗口（探针用）
#[allow(dead_code)]
pub fn find_overlay() -> usize {
    unsafe {
        let mut name: Vec<u16> = "SmartPasteOverlay\0".encode_utf16().collect();
        windows_sys::Win32::UI::WindowsAndMessaging::FindWindowW(
            name.as_mut_ptr(),
            std::ptr::null(),
        ) as usize
    }
}

/// 显示/隐藏遮罩（探针用）
#[allow(dead_code)]
pub fn show(hwnd: usize, visible: bool) {
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(
            hwnd as HWND,
            if visible { 5 } else { 0 },
        );
    }
}
