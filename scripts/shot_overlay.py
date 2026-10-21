"""截一张截图遮罩的完整状态（选框 + 尺寸标签 + 工具条）。

用程序自带的 --rect 入口直接锁定选区，不模拟鼠标——
模拟鼠标在 DPI 缩放下坐标会飘，截图位置对不上。

用法：
    python scripts/shot_overlay.py out.png                             # 默认 620,420,560,340
    python scripts/shot_overlay.py out.png 900 300 560 340
    python scripts/shot_overlay.py out.png 900 300 560 340 1200 700    # 最后两个是鼠标停留点，测悬停
"""

import ctypes
import subprocess
import sys
import time
from pathlib import Path

from PIL import ImageGrab

ROOT = Path(__file__).resolve().parent.parent
EXE = ROOT / "target" / "release" / "smartpaste.exe"

u = ctypes.windll.user32
try:
    ctypes.windll.shcore.SetProcessDpiAwareness(2)
except OSError:
    pass

VK_ESCAPE = 0x1B
KEYEVENTF_KEYUP = 0x0002


def main():
    out = Path(sys.argv[1])
    x = int(sys.argv[2]) if len(sys.argv) > 2 else 620
    y = int(sys.argv[3]) if len(sys.argv) > 3 else 420
    w = int(sys.argv[4]) if len(sys.argv) > 4 else 560
    h = int(sys.argv[5]) if len(sys.argv) > 5 else 340
    hover = (int(sys.argv[6]), int(sys.argv[7])) if len(sys.argv) > 7 else None

    proc = subprocess.Popen(
        [str(EXE), "--auto", "--rect", str(x), str(y), str(w), str(h)]
    )
    time.sleep(2.4)
    if hover:
        u.SetCursorPos(hover[0], hover[1])
        time.sleep(0.5)
    else:
        u.SetCursorPos(x + w // 2, y + h // 2)
        time.sleep(0.3)
    ImageGrab.grab().save(out)

    u.keybd_event(VK_ESCAPE, 0, 0, 0)
    u.keybd_event(VK_ESCAPE, 0, KEYEVENTF_KEYUP, 0)
    time.sleep(0.4)

    # Popen.kill() 偶尔留残进程，一旦留下单实例锁，后面的测试全起不来
    proc.kill()
    try:
        proc.wait(timeout=2)
    except subprocess.TimeoutExpired:
        subprocess.run(
            ["taskkill", "/F", "/PID", str(proc.pid)],
            capture_output=True,
            check=False,
        )

    print(f"saved {out}  rect=({x},{y},{w},{h})")


if __name__ == "__main__":
    main()
