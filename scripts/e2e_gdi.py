"""GDI 版端到端测试：热键 -> 遮罩 -> 拖拽 -> Enter 复制 -> 校验剪贴板。"""
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

VK_CTRL, VK_ALT, VK_X, VK_RETURN, VK_ESC = 0x11, 0x12, 0x58, 0x0D, 0x1B
KEYUP = 2
LEFTDOWN, LEFTUP = 0x0002, 0x0004
CF_DIB = 8

EXE = r"E:\Programs\AIWorkSpace\smartpaste\target\release\smartpaste.exe"
LOG = os.path.join(os.environ.get("TEMP", "."), "smartpaste.log")


class POINT(ctypes.Structure):
    _fields_ = [("x", ctypes.c_long), ("y", ctypes.c_long)]


def key(vk, up=False):
    u.keybd_event(vk, 0, KEYUP if up else 0, 0)


def fire_hotkey():
    key(VK_CTRL); key(VK_ALT); key(VK_X)
    key(VK_X, True); key(VK_ALT, True); key(VK_CTRL, True)


def clipboard_dib_size():
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


def read_log():
    if not os.path.exists(LOG):
        return "(no log)"
    with open(LOG, "r", encoding="utf-8", errors="replace") as f:
        return f.read().strip()


if os.path.exists(LOG):
    os.remove(LOG)

pt = POINT()
u.GetCursorPos(ctypes.byref(pt))
saved = (pt.x, pt.y)

proc = subprocess.Popen([EXE])
time.sleep(1.5)
print(f"process alive: {proc.poll() is None}")

try:
    print("firing hotkey ...")
    fire_hotkey()
    time.sleep(1.0)

    hwnd = u.FindWindowW("SmartPasteOverlay", None)
    print(f"overlay hwnd: {hwnd}  (0 means NOT created)")

    print("dragging ...")
    u.SetCursorPos(400, 400)
    time.sleep(0.1)
    u.mouse_event(LEFTDOWN, 0, 0, 0, 0)
    time.sleep(0.1)
    for i in range(1, 16):
        u.SetCursorPos(400 + i * 30, 400 + i * 18)
        time.sleep(0.012)
    u.mouse_event(LEFTUP, 0, 0, 0, 0)
    time.sleep(0.5)

    print("pressing Enter ...")
    key(VK_RETURN)
    key(VK_RETURN, True)
    time.sleep(1.0)

    hwnd2 = u.FindWindowW("SmartPasteOverlay", None)
    print(f"overlay after Enter: {hwnd2} (0 = closed)")
    print(f"clipboard DIB bytes: {clipboard_dib_size()}")
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
