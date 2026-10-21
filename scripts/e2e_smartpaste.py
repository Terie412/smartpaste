"""端到端冒烟测试：热键 -> 框选拖拽 -> Enter 复制 -> 读日志验证。"""
import ctypes
import os
import subprocess
import sys
import time

try:
    ctypes.windll.shcore.SetProcessDpiAwareness(2)
except Exception:
    ctypes.windll.user32.SetProcessDPIAware()

u = ctypes.windll.user32
VK_CTRL, VK_ALT, VK_X, VK_RETURN = 0x11, 0x12, 0x58, 0x0D
KEYUP = 2
LEFTDOWN, LEFTUP = 0x0002, 0x0004


class POINT(ctypes.Structure):
    _fields_ = [("x", ctypes.c_long), ("y", ctypes.c_long)]


def key(vk, up=False):
    u.keybd_event(vk, 0, KEYUP if up else 0, 0)


EXE = r"E:\Programs\AIWorkSpace\smartpaste\target\release\smartpaste.exe"
LOG = os.path.join(os.environ.get("TEMP", "."), "smartpaste.log")

if os.path.exists(LOG):
    os.remove(LOG)


def read_log():
    if not os.path.exists(LOG):
        return "(no log)"
    with open(LOG, "r", encoding="utf-8", errors="replace") as f:
        return f.read().strip()


pt = POINT()
u.GetCursorPos(ctypes.byref(pt))
saved = (pt.x, pt.y)
print(f"cursor saved at {saved}")

proc = subprocess.Popen([EXE])
time.sleep(2.0)

try:
    print("firing hotkey Ctrl+Alt+X ...")
    key(VK_CTRL)
    key(VK_ALT)
    key(VK_X)
    key(VK_X, True)
    key(VK_ALT, True)
    key(VK_CTRL, True)
    time.sleep(1.0)

    print("dragging 400,400 -> 900,700 ...")
    u.SetCursorPos(400, 400)
    time.sleep(0.08)
    u.mouse_event(LEFTDOWN, 0, 0, 0, 0)
    time.sleep(0.08)
    for i in range(1, 21):
        u.SetCursorPos(400 + i * 25, 400 + i * 15)
        time.sleep(0.012)
    time.sleep(0.08)
    u.mouse_event(LEFTUP, 0, 0, 0, 0)
    time.sleep(0.4)

    print("pressing Enter ...")
    key(VK_RETURN)
    key(VK_RETURN, True)
    time.sleep(1.2)

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
