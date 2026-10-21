"""诊断：遮罩窗口是否可见、是否在最顶层、鼠标落点归属哪个窗口。"""
import ctypes
import os
import subprocess
import time

try:
    ctypes.windll.shcore.SetProcessDpiAwareness(2)
except Exception:
    ctypes.windll.user32.SetProcessDPIAware()

u = ctypes.windll.user32
u.FindWindowW.argtypes = [ctypes.c_wchar_p, ctypes.c_wchar_p]
u.FindWindowW.restype = ctypes.c_void_p
u.WindowFromPoint.argtypes = [ctypes.c_long, ctypes.c_long]
u.WindowFromPoint.restype = ctypes.c_void_p
u.GetClassNameW.argtypes = [ctypes.c_void_p, ctypes.c_wchar_p, ctypes.c_int]

EXE = r"E:\Programs\AIWorkSpace\smartpaste\target\release\smartpaste.exe"


class RECT(ctypes.Structure):
    _fields_ = [("left", ctypes.c_long), ("top", ctypes.c_long),
                ("right", ctypes.c_long), ("bottom", ctypes.c_long)]


def cls(hwnd):
    if not hwnd:
        return "(null)"
    buf = ctypes.create_unicode_buffer(256)
    u.GetClassNameW(hwnd, buf, 256)
    return buf.value


def rect_of(hwnd):
    r = RECT()
    u.GetWindowRect(hwnd, ctypes.byref(r))
    return (r.left, r.top, r.right - r.left, r.bottom - r.top)


proc = subprocess.Popen([EXE, "--auto"])
time.sleep(3.0)

try:
    hwnd = u.FindWindowW("SmartPasteOverlay", None)
    print(f"overlay hwnd   : {hwnd}")
    if hwnd:
        print(f"  class        : {cls(hwnd)}")
        print(f"  rect         : {rect_of(hwnd)}")
        print(f"  IsVisible    : {u.IsWindowVisible(hwnd)}")
        print(f"  IsIconic     : {u.IsIconic(hwnd)}")
        print(f"  exstyle      : {hex(u.GetWindowLongW(hwnd, -20) & 0xFFFFFFFF)}")
    print(f"foreground     : {u.GetForegroundWindow()} ({cls(u.GetForegroundWindow())})")
    for pt in [(400, 400), (1200, 700), (430, 725)]:
        w = u.WindowFromPoint(pt[0], pt[1])
        print(f"WindowFromPoint{pt} -> {w} ({cls(w)})")
finally:
    proc.terminate()
    try:
        proc.wait(timeout=3)
    except Exception:
        proc.kill()
