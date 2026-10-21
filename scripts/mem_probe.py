"""测常驻 / 遮罩打开 / 截图完成后 三个时点的内存。"""
import ctypes
import csv
import io
import os
import subprocess
import time

try:
    ctypes.windll.shcore.SetProcessDpiAwareness(2)
except Exception:
    ctypes.windll.user32.SetProcessDPIAware()

u = ctypes.windll.user32
VK_CTRL, VK_ALT, VK_X, VK_ESC = 0x11, 0x12, 0x58, 0x1B
KEYUP = 2
EXE = r"E:\Programs\AIWorkSpace\smartpaste\target\release\smartpaste.exe"


def key(vk, up=False):
    u.keybd_event(vk, 0, KEYUP if up else 0, 0)


def mem_kb(pid):
    out = subprocess.check_output(
        ["tasklist", "/FI", f"PID eq {pid}", "/FO", "CSV", "/NH"],
        stderr=subprocess.DEVNULL,
    ).decode("gbk", errors="ignore")
    rows = list(csv.reader(io.StringIO(out.strip())))
    if not rows or len(rows[0]) < 5:
        return None
    raw = rows[0][-1].replace(",", "").replace("K", "").replace("k", "").strip()
    return int(raw)


proc = subprocess.Popen([EXE])
time.sleep(2.0)
base = mem_kb(proc.pid)
print(f"idle            : {base/1024:.1f} MB")

key(VK_CTRL); key(VK_ALT); key(VK_X)
key(VK_X, True); key(VK_ALT, True); key(VK_CTRL, True)
time.sleep(1.2)
overlay = mem_kb(proc.pid)
print(f"overlay open    : {overlay/1024:.1f} MB")

u.SetCursorPos(400, 400)
time.sleep(0.1)
u.mouse_event(0x0002, 0, 0, 0, 0)
time.sleep(0.1)
for i in range(1, 15):
    u.SetCursorPos(400 + i * 30, 400 + i * 18)
    time.sleep(0.015)
u.mouse_event(0x0004, 0, 0, 0, 0)
time.sleep(0.5)
drag = mem_kb(proc.pid)
print(f"after selection : {drag/1024:.1f} MB")

key(VK_ESC); key(VK_ESC, True)
time.sleep(1.5)
after = mem_kb(proc.pid)
print(f"back to idle    : {after/1024:.1f} MB")

proc.terminate()
try:
    proc.wait(timeout=3)
except Exception:
    proc.kill()
