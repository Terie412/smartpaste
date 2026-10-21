"""GDI 版完整验证：自投递热键 -> 拖拽框选 -> 点工具栏「复制」-> 校验剪贴板。"""
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
u.GetClipboardData.restype = ctypes.c_void_p
u.OpenClipboard.argtypes = [ctypes.c_void_p]
u.OpenClipboard.restype = ctypes.c_int

EXE = r"E:\Programs\AIWorkSpace\smartpaste\target\release\smartpaste.exe"
LOG = os.path.join(os.environ.get("TEMP", "."), "smartpaste.log")
CF_DIB = 8
LEFTDOWN, LEFTUP = 0x0002, 0x0004


class POINT(ctypes.Structure):
    _fields_ = [("x", ctypes.c_long), ("y", ctypes.c_long)]


def clip_size():
    if not u.OpenClipboard(None):
        return None
    try:
        h = u.GetClipboardData(CF_DIB)
        if not h:
            return 0
        k32 = ctypes.windll.kernel32
        k32.GlobalSize.argtypes = [ctypes.c_void_p]
        k32.GlobalSize.restype = ctypes.c_size_t
        return k32.GlobalSize(h)
    finally:
        u.CloseClipboard()


def clear_clip():
    if u.OpenClipboard(None):
        u.EmptyClipboard()
        u.CloseClipboard()


def read_log():
    if not os.path.exists(LOG):
        return "(no log)"
    with open(LOG, "r", encoding="utf-8", errors="replace") as f:
        return f.read().strip()


if os.path.exists(LOG):
    os.remove(LOG)
clear_clip()

pt = POINT()
u.GetCursorPos(ctypes.byref(pt))
saved = (pt.x, pt.y)

proc = subprocess.Popen([EXE, "--auto"])
time.sleep(3.0)

try:
    hwnd = u.FindWindowW("SmartPasteOverlay", None)
    print(f"overlay hwnd: {hwnd}")

    print("drag 400,400 -> 900,700")
    u.SetCursorPos(400, 400)
    time.sleep(0.15)
    u.mouse_event(LEFTDOWN, 0, 0, 0, 0)
    time.sleep(0.1)
    for i in range(1, 21):
        u.SetCursorPos(400 + i * 25, 400 + i * 15)
        time.sleep(0.012)
    time.sleep(0.1)
    u.mouse_event(LEFTUP, 0, 0, 0, 0)
    time.sleep(0.6)

    print("click toolbar '复制' at (430, 725)")
    u.SetCursorPos(430, 725)
    time.sleep(0.15)
    u.mouse_event(LEFTDOWN, 0, 0, 0, 0)
    time.sleep(0.08)
    u.mouse_event(LEFTUP, 0, 0, 0, 0)
    time.sleep(1.0)

    print(f"overlay after click: {u.FindWindowW('SmartPasteOverlay', None)} (None = closed)")
    print(f"clipboard DIB bytes: {clip_size()}   expected = 600040")
    print("--- log ---")
    print(read_log())
finally:
    u.SetCursorPos(saved[0], saved[1])
    proc.terminate()
    try:
        proc.wait(timeout=3)
    except Exception:
        proc.kill()
    print("done")
