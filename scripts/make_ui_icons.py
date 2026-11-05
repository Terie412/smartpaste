"""把 assets/icons.svg 渲成 assets/icons.png（十一格横向雪碧图）。

图标是 Lucide 的线性风格：24x24 网格、2px 线宽、圆头圆角。
这里按 4 倍渲染成 1056x96，运行时再缩到 32px 显示，
留足余量，高 DPI 下也不会糊。

渲完会把每格的墨水外接框打出来（含描边，换算成 24 网格单位），
和 TARGET 对不上的会标 <<——渲染尺寸一样不代表看起来一样大。

用法：python scripts/make_ui_icons.py
"""

import subprocess
import tempfile
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "assets"

BROWSERS = [
    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files\Google\Chrome\Application\chrome.exe",
]

WIDTH, HEIGHT = 1152, 96
# 每格边长，也是 SVG 里 24 单位网格的 4 倍
CELL = 96

# 期望的墨水尺寸（宽, 高；24 格单位）。见 AGENTS.md 8.1：
# 方形基准 18~19、圆略大、斜向收小、纵长/宽扁保持自身比例。
EXPECT = [
    ("copy", 19.0, 19.0),
    ("save", 18.5, 18.5),
    ("pin", 14.5, 20.0),
    ("arrow", 15.5, 15.5),
    ("rect", 18.0, 18.0),
    ("circle", 19.0, 19.0),
    ("pen", 18.5, 18.5),
    ("text", 17.5, 18.5),
    ("eraser", 19.5, 18.0),
    ("record", 21.5, 15.0),
    ("palette", 20.0, 20.0),
    ("scroll", 15.2, 17.8),
]

# alpha 低于这个值当背景，和图标自身的抗锯齿边缘区分开
INK_MIN = 40
# 偏差超过这个值（单位）就标出来
TOL = 1.0


def find_browser() -> str:
    for p in BROWSERS:
        if Path(p).is_file():
            return p
    raise SystemExit("没找到 Edge/Chrome")


def ink_box(cell: Image.Image) -> tuple[float, float, float, float] | None:
    """一格里墨水的外接框，回 (宽, 高, 中心x, 中心y)，单位是 24 网格。"""
    a = cell.split()[3].load()
    xs, ys = [], []
    for y in range(cell.height):
        for x in range(cell.width):
            if a[x, y] >= INK_MIN:
                xs.append(x)
                ys.append(y)
    if not xs:
        return None
    u = 24 / cell.width
    return (
        (max(xs) - min(xs) + 1) * u,
        (max(ys) - min(ys) + 1) * u,
        (min(xs) + max(xs) + 1) / 2 * u,
        (min(ys) + max(ys) + 1) / 2 * u,
    )


def report(im: Image.Image) -> int:
    """打印每格的视觉尺寸体检表，回离群格数。"""
    im = im.convert("RGBA")
    print(f"{'#':<3}{'名称':<9}{'实际 w×h':<15}{'期望 w×h':<15}{'中心':<14}备注")
    off = 0
    for i in range(im.width // CELL):
        name, tw, th = EXPECT[i] if i < len(EXPECT) else (f"?{i}", 0.0, 0.0)
        box = ink_box(im.crop((i * CELL, 0, i * CELL + CELL, CELL)))
        if box is None:
            print(f"{i:<3}{name:<9}{'空白':<15}{'':<15}{'':<14}!!")
            off += 1
            continue
        w, h, cx, cy = box
        note = ""
        if tw:
            if abs(w - tw) > TOL or abs(h - th) > TOL:
                note = f"<< 差 {w - tw:+.1f} / {h - th:+.1f}"
                off += 1
        else:
            note = "没有期望值"
        if abs(cx - 12) > TOL or abs(cy - 12) > TOL:
            note += " 偏心"
            off += 1
        print(
            f"{i:<3}{name:<9}{f'{w:.1f}×{h:.1f}':<15}"
            f"{(f'{tw:.1f}×{th:.1f}' if tw else '-'):<15}"
            f"{f'({cx:.1f},{cy:.1f})':<14}{note}"
        )
    return off


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

    im = Image.open(out)
    print(f"{out.name}: {im.size}")
    off = report(im)
    print("视觉尺寸全部达标" if off == 0 else f"{off} 处需要处理")


if __name__ == "__main__":
    main()

