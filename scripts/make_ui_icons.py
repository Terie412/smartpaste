"""把 assets/icons.svg 渲成 assets/icons.png（三格横向雪碧图）。

图标是 Lucide 的线性风格：24x24 网格、2px 线宽、圆头圆角。
这里按 4 倍渲染成 288x96，运行时再缩到 20px 显示，
留足余量，高 DPI 下也不会糊。

用法：python scripts/make_ui_icons.py
"""

import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "assets"

BROWSERS = [
    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files\Google\Chrome\Application\chrome.exe",
]

WIDTH, HEIGHT = 864, 96


def find_browser() -> str:
    for p in BROWSERS:
        if Path(p).is_file():
            return p
    raise SystemExit("没找到 Edge/Chrome")


def main() -> None:
    out = ASSETS / "icons.png"
    html = (
        '<!doctype html><meta charset="utf-8">'
        "<style>html,body{margin:0;padding:0;background:transparent}"
        f"img{{display:block;width:{WIDTH}px;height:{HEIGHT}px}}</style>"
        f'<img src="{(ASSETS / "icons.svg").as_uri()}">'
    )
    with tempfile.TemporaryDirectory() as tmp:
        page = Path(tmp) / "r.html"
        page.write_text(html, encoding="utf-8")
        subprocess.run(
            [
                find_browser(),
                "--headless=new",
                "--disable-gpu",
                "--hide-scrollbars",
                "--default-background-color=00000000",
                "--force-device-scale-factor=1",
                f"--window-size={WIDTH},{HEIGHT}",
                f"--screenshot={out}",
                page.as_uri(),
            ],
            check=True,
            capture_output=True,
        )

    from PIL import Image

    im = Image.open(out)
    alpha = im.convert("RGBA").getchannel("A")
    print(f"{out.name}: {im.size}, 非透明像素 {sum(1 for p in alpha.getdata() if p)}")


if __name__ == "__main__":
    main()
