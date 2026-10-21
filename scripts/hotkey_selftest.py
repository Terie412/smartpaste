"""最小热键监听器：验证 keybd_event 注入能否触发 RegisterHotKey。"""
import ctypes
import threading
import time
from ctypes import wintypes

u = ctypes.windll.user32
u.RegisterHotKey.argtypes = [wintypes.HWND, ctypes.c_int, wintypes.UINT, wintypes.UINT]
u.RegisterHotKey.restype = wintypes.BOOL
u.PeekMessageW.argtypes = [ctypes.POINTER(wintypes.MSG), wintypes.HWND, wintypes.UINT, wintypes.UINT, wintypes.UINT]
u.PeekMessageW.restype = wintypes.BOOL

ULONG_PTR = ctypes.c_ulonglong if ctypes.sizeof(ctypes.c_void_p) == 8 else ctypes.c_ulong


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [
        ("wVk", wintypes.WORD),
        ("wScan", wintypes.WORD),
        ("dwFlags", wintypes.DWORD),
        ("time", wintypes.DWORD),
        ("dwExtraInfo", ULONG_PTR),
    ]


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [
        ("dx", wintypes.LONG),
        ("dy", wintypes.LONG),
        ("mouseData", wintypes.DWORD),
        ("dwFlags", wintypes.DWORD),
        ("time", wintypes.DWORD),
        ("dwExtraInfo", ULONG_PTR),
    ]


class _INPUTUNION(ctypes.Union):
    _fields_ = [("ki", KEYBDINPUT), ("mi", MOUSEINPUT)]


class INPUT(ctypes.Structure):
    _fields_ = [("type", wintypes.DWORD), ("u", _INPUTUNION)]


VK_CTRL, VK_ALT, VK_X = 0x11, 0x12, 0x58


def send_input_combo():
    seq = [(VK_CTRL, 0), (VK_ALT, 0), (VK_X, 0), (VK_X, 2), (VK_ALT, 2), (VK_CTRL, 2)]
    for vk, flags in seq:
        inp = INPUT(type=1)
        inp.u.ki = KEYBDINPUT(wVk=vk, wScan=0, dwFlags=flags, time=0, dwExtraInfo=0)
        u.SendInput(1, ctypes.byref(inp), ctypes.sizeof(INPUT))
        time.sleep(0.05)


def keybd_combo():
    seq = [(VK_CTRL, 0), (VK_ALT, 0), (VK_X, 0), (VK_X, 2), (VK_ALT, 2), (VK_CTRL, 2)]
    for vk, flags in seq:
        u.keybd_event(vk, 0, flags, 0)
        time.sleep(0.05)


ok = u.RegisterHotKey(None, 1, 0x3, 0x58)  # MOD_ALT | MOD_CONTROL
print(f"self RegisterHotKey -> {ok} (1 = ok)")
if not ok:
    print("热键已被其它程序占用，先杀掉 smartpaste")
    raise SystemExit(1)


def trigger():
    time.sleep(0.5)
    print("SendInput ...")
    send_input_combo()
    time.sleep(0.8)
    print("keybd_event ...")
    keybd_combo()


threading.Thread(target=trigger, daemon=True).start()

deadline = time.time() + 4.0
got = False
while time.time() < deadline:
    msg = wintypes.MSG()
    if u.PeekMessageW(ctypes.byref(msg), None, 0, 0, 1):
        if msg.message == 0x0312:
            print(f"GOT WM_HOTKEY  id={msg.wParam}")
            got = True
            break
    time.sleep(0.02)

u.UnregisterHotKey(None, 1)
print("result:", "TRIGGERED" if got else "NOT TRIGGERED")
