"""只验热键：多种注入方式，看哪种能触发。"""
import ctypes
import os
import subprocess
import time

try:
    ctypes.windll.shcore.SetProcessDpiAwareness(2)
except Exception:
    ctypes.windll.user32.SetProcessDPIAware()

u = ctypes.windll.user32
u.MapVirtualKeyW.argtypes = [ctypes.c_uint, ctypes.c_uint]
u.MapVirtualKeyW.restype = ctypes.c_uint

EXE = r"E:\Programs\AIWorkSpace\smartpaste\target\release\smartpaste.exe"
LOG = os.path.join(os.environ.get("TEMP", "."), "smartpaste.log")

VK_CTRL, VK_ALT, VK_X = 0x11, 0x12, 0x58


def key_scan(vk, up=False):
    sc = u.MapVirtualKeyW(vk, 0)
    u.keybd_event(vk, sc, 2 if up else 0, 0)


def combo_scan():
    key_scan(VK_CTRL); time.sleep(0.06)
    key_scan(VK_ALT); time.sleep(0.06)
    key_scan(VK_X); time.sleep(0.08)
    key_scan(VK_X, True); time.sleep(0.04)
    key_scan(VK_ALT, True); time.sleep(0.04)
    key_scan(VK_CTRL, True)


def combo_plain():
    u.keybd_event(VK_CTRL, 0, 0, 0); time.sleep(0.06)
    u.keybd_event(VK_ALT, 0, 0, 0); time.sleep(0.06)
    u.keybd_event(VK_X, 0, 0, 0); time.sleep(0.08)
    u.keybd_event(VK_X, 0, 2, 0); time.sleep(0.04)
    u.keybd_event(VK_ALT, 0, 2, 0); time.sleep(0.04)
    u.keybd_event(VK_CTRL, 0, 2, 0)


def read_log():
    if not os.path.exists(LOG):
        return ""
    with open(LOG, "r", encoding="utf-8", errors="replace") as f:
        return f.read()


if os.path.exists(LOG):
    os.remove(LOG)

proc = subprocess.Popen([EXE])
time.sleep(2.0)

try:
    print("--- combo_scan ---")
    combo_scan()
    time.sleep(1.0)
    print(read_log() or "(empty)")

    print("--- combo_plain ---")
    combo_plain()
    time.sleep(1.0)
    print(read_log() or "(empty)")
finally:
    proc.terminate()
    try:
        proc.wait(timeout=3)
    except Exception:
        proc.kill()
