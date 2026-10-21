"""重新生成 assets/smartpaste.ico（以及导出的 png）。

流程：headless Edge 把 SVG 渲染成 512x512 PNG → Pillow 缩放到各尺寸 → 打包 ICO。
小尺寸（16/20/24）用 smartpaste-small.svg，那版的趾垫间隙是按 16px 重新排的；
32px 以上用主图，保留完整爪形。

帧一律写 BMP/DIB 而不是 PNG 压缩，这样 CreateIconFromResourceEx
在任何 Windows 版本上都能直接吃。

用法（工作目录随意）：
    python scripts/make_icon.py
"""

import struct
import subprocess
import tempfile
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "assets"

EDGE_CANDIDATES = [
    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files\Google\Chrome\Application\chrome.exe",
]

# (尺寸, 用哪张源图)：小尺寸走简化版
PLAN = [
    (16, "small"),
    (20, "small"),
    (24, "small"),
    (32, "main"),
    (40, "main"),
    (48, "main"),
    (64, "main"),
    (128, "main"),
    (256, "main"),
]

SOURCES = {"main": "smartpaste.svg", "small": "smartpaste-small.svg"}


def find_browser() -> str:
    for p in EDGE_CANDIDATES:
        if Path(p).is_file():
            return p
    raise SystemExit("没找到 Edge/Chrome，无法渲染 SVG")


def render(svg: str, out: Path) -> None:
    """把 SVG 渲成 512x512 PNG。包一层 HTML，避免浏览器给 SVG 加白边。

    SVG 必须用绝对 file:// URI：HTML 写在临时目录里，相对路径指不过去。
    """
    html = (
        '<!doctype html><meta charset="utf-8">'
        "<style>html,body{margin:0;padding:0;background:transparent}"
        "img{display:block;width:512px;height:512px}</style>"
        f'<img src="{(ASSETS / svg).as_uri()}">'
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
                # 关键：不给这个参数，圆角外的透明区会被填成白色，
                # 图标贴到深色任务栏上就是个白方块
                "--default-background-color=00000000",
                "--force-device-scale-factor=1",
                "--window-size=512,512",
                f"--screenshot={out}",
                page.as_uri(),
            ],
            check=True,
            capture_output=True,
        )


def bmp_frame(img: Image.Image) -> bytes:
    """单帧图标数据：BITMAPINFOHEADER + bottom-up BGRA + 空 AND mask。"""
    w, h = img.size
    px = img.convert("RGBA").load()

    rows = []
    for y in range(h - 1, -1, -1):  # ICO 里的 DIB 是 bottom-up
        row = bytearray()
        for x in range(w):
            r, g, b, a = px[x, y]
            row += bytes((b, g, r, a))
        rows.append(bytes(row))
    xor = b"".join(rows)

    stride = ((w + 31) // 32) * 4
    and_mask = bytes(stride * h)  # 全 0：透明度交给 alpha 通道

    header = struct.pack(
        "<IiiHHIIiiII",
        40,        # biSize
        w,         # biWidth
        h * 2,     # biHeight = XOR + AND mask
        1,         # biPlanes
        32,        # biBitCount
        0,         # BI_RGB
        len(xor) + len(and_mask),
        0, 0, 0, 0,
    )
    return header + xor + and_mask


def main() -> None:
    rendered = {}
    with tempfile.TemporaryDirectory() as tmp:
        for key, svg in SOURCES.items():
            png = Path(tmp) / f"{key}.png"
            render(svg, png)
            rendered[key] = Image.open(png).convert("RGBA")

    frames = []
    for size, key in PLAN:
        frames.append((size, bmp_frame(rendered[key].resize((size, size), Image.LANCZOS))))

    offset = 6 + 16 * len(frames)
    entries, blobs = [], []
    for size, data in frames:
        dim = 0 if size >= 256 else size  # 256 在目录项里记作 0
        entries.append(struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset))
        blobs.append(data)
        offset += len(data)

    out = ASSETS / "smartpaste.ico"
    out.write_bytes(struct.pack("<HHH", 0, 1, len(frames)) + b"".join(entries) + b"".join(blobs))

    # 顺手导出大图，方便外部引用
    for size in (256, 512):
        rendered["main"].resize((size, size), Image.LANCZOS).save(
            ASSETS / f"smartpaste-{size}.png"
        )

    print(f"{out.name}: {len(frames)} 帧, {out.stat().st_size / 1024:.1f} KiB")
    for size, data in frames:
        print(f"  {size:>3}x{size:<3} {len(data) / 1024:>7.1f} KiB")


if __name__ == "__main__":
    main()
