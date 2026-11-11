#!/usr/bin/env python3
"""生成品牌图：docs/images/icon-256.png、icon-512.png、social-preview.png，
以及应用自己用的 assets/AppIcon-1024.png（打包时转成 .icns）和
crates/rind-app/assets/logo.png（界面里的标志）。

依赖 Pillow（`python3 -m pip install pillow`）与 macOS 自带字体
（SF Rounded / SF / SF Mono）。文件在任意位置跑都可以，输出落在仓库
docs/images/ 下：

- icon-*：黄底圆角方格 + 深色 "R"。给 awesome 列表的 icon_url 用。
- social-preview.png：1280x640，供 GitHub 仓库 Settings -> Social preview
  手动上传（API 不支持设置社交预览图）。
- AppIcon-1024.png：按 macOS 应用图标网格画——1024 画布里方格占 824、四周留 100，
  带一层柔和投影，放进 Dock 时和系统 App 一样大。`scripts/package-dmg.sh` 用
  sips + iconutil 把它转成 AppIcon.icns。
- logo.png：不留边、不带投影的方格（256px），界面侧栏和设置页里显示。
- banner-en.png / banner-zh.png：1280x300 的 README 顶部横幅，Logo + 名字 + 一句话介绍。

用法：python3 scripts/make-brand-images.py
"""

import math
import pathlib

from PIL import Image, ImageDraw, ImageFilter, ImageFont

REPO = pathlib.Path(__file__).resolve().parent.parent
OUT_DIR = REPO / "docs" / "images"
APP_ICON = REPO / "assets" / "AppIcon-1024.png"
LOGO = REPO / "crates" / "rind-app" / "assets" / "logo.png"

INK = (11, 11, 12, 255)  # 近黑，跟应用深色主题一致
TILE_TOP = (255, 214, 74)
TILE_BOTTOM = (239, 174, 0)
BG = (11, 11, 12)

SS = 4  # 超采样倍率

SF_ROUNDED = "/System/Library/Fonts/SFNSRounded.ttf"
SF = "/System/Library/Fonts/SFNS.ttf"
SF_MONO = "/System/Library/Fonts/SFNSMono.ttf"
PINGFANG = "/System/Library/Fonts/Hiragino Sans GB.ttc"


def font(path: str, size: int, variation: str | None = None) -> ImageFont.FreeTypeFont:
    f = ImageFont.truetype(path, size)
    if variation:
        f.set_variation_by_name(variation)
    return f


def rounded_tile(size: int) -> Image.Image:
    """黄底圆角方格，顶部带一点高光。"""
    s = size * SS
    grad = Image.new("RGB", (s, s))
    d = ImageDraw.Draw(grad)
    for y in range(s):
        t = y / (s - 1)
        c = tuple(int(TILE_TOP[i] + (TILE_BOTTOM[i] - TILE_TOP[i]) * t) for i in range(3))
        d.line([(0, y), (s, y)], fill=c)
    glaze = Image.new("L", (s, s), 0)
    gd = ImageDraw.Draw(glaze)
    for y in range(int(s * 0.40)):
        gd.line([(0, y), (s, y)], fill=int(38 * (1 - y / (s * 0.40))))
    grad = Image.composite(Image.new("RGB", (s, s), (255, 255, 255)), grad, glaze)
    mask = Image.new("L", (s, s), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, s - 1, s - 1], radius=int(s * 0.235), fill=255)
    tile = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    tile.paste(grad, (0, 0), mask)
    return tile


def render_icon(size: int) -> Image.Image:
    s = size * SS
    tile = rounded_tile(size)
    d = ImageDraw.Draw(tile)
    f = font(SF_ROUNDED, int(s * 0.60), "Bold")
    d.text((s * 0.502, s * 0.508), "R", font=f, fill=INK, anchor="mm")
    return tile.resize((size, size), Image.LANCZOS)


def render_app_icon() -> Image.Image:
    """macOS 应用图标网格：1024 画布，824 的方格居中，下方一层柔和投影。"""
    canvas, tile_size, margin = 1024, 824, 100
    img = Image.new("RGBA", (canvas, canvas), (0, 0, 0, 0))
    shadow = Image.new("L", (canvas, canvas), 0)
    ImageDraw.Draw(shadow).rounded_rectangle(
        [margin, margin + 12, margin + tile_size, margin + tile_size + 12],
        radius=int(tile_size * 0.235),
        fill=90,
    )
    shadow = shadow.filter(ImageFilter.GaussianBlur(14))
    img.paste(Image.new("RGBA", (canvas, canvas), (0, 0, 0, 255)), (0, 0), shadow)
    tile = render_icon(tile_size)
    img.alpha_composite(tile, (margin, margin))
    return img


def radial_glow(size: tuple[int, int], center: tuple[float, float], radius: float, color: tuple[int, int, int], peak: float) -> Image.Image:
    """小图逐像素算完再放大，便宜且平滑。"""
    w, h = size
    small = Image.new("RGB", (w // 4, h // 4))
    px = small.load()
    for y in range(h // 4):
        for x in range(w // 4):
            dx = x * 4 - center[0]
            dy = y * 4 - center[1]
            t = math.hypot(dx, dy) / radius
            k = max(0.0, 1.0 - t) ** 2 * peak
            px[x, y] = (
                int(BG[0] + (color[0] - BG[0]) * k),
                int(BG[1] + (color[1] - BG[1]) * k),
                int(BG[2] + (color[2] - BG[2]) * k),
            )
    return small.resize(size, Image.BICUBIC)


def render_social() -> Image.Image:
    w, h = 1280, 640
    img = radial_glow((w, h), (256, 320), 560, (240, 182, 0), 0.16)
    tile = render_icon(256)
    img.paste(tile, (128, 192), tile)

    d = ImageDraw.Draw(img)
    title_font = font(SF, 148, "Bold")
    d.text((442, 256), "Rind", font=title_font, fill=(245, 245, 247), anchor="lm")
    d.text(
        (448, 366),
        "Explainable, undoable macOS uninstaller",
        font=font(SF, 42, "Regular"),
        fill=(166, 166, 174),
        anchor="lm",
    )
    d.text(
        (449, 438),
        "Rust · GPUI · GPL-3.0 · macOS 14+",
        font=font(SF_MONO, 26, "Regular"),
        fill=(117, 117, 125),
        anchor="lm",
    )
    return img


def main() -> None:
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    for size in (256, 512):
        render_icon(size).save(OUT_DIR / f"icon-{size}.png")
    render_social().save(OUT_DIR / "social-preview.png")
    APP_ICON.parent.mkdir(parents=True, exist_ok=True)
    render_app_icon().save(APP_ICON)
    LOGO.parent.mkdir(parents=True, exist_ok=True)
    render_icon(256).save(LOGO)
    render_banner(
        "Explainable, undoable macOS uninstaller",
        "Rust \u00b7 GPUI \u00b7 GPL-3.0 \u00b7 macOS 14+",
        OUT_DIR / "banner-en.png",
    )
    render_banner(
        "\u53ef\u89e3\u91ca\u3001\u53ef\u64a4\u9500\u7684 macOS \u5378\u8f7d\u5de5\u5177",
        "Rust \u00b7 GPUI \u00b7 GPL-3.0 \u00b7 macOS 14+",
        OUT_DIR / "banner-zh.png",
        tagline_font=PINGFANG,
    )
    outputs = [OUT_DIR / n for n in ("icon-256.png", "icon-512.png", "social-preview.png", "banner-en.png", "banner-zh.png")]
    for p in outputs + [APP_ICON, LOGO]:
        print(f"{p.relative_to(REPO)}  {p.stat().st_size:,} bytes")


def render_banner(tagline: str, subtitle: str, out: pathlib.Path, tagline_font: str = SF) -> None:
    """README 顶部的横幅：Logo + 名字 + 一句话介绍，深色底 + 柔光。"""
    w, h = 1280, 300
    img = radial_glow((w, h), (280, 150), 620, (240, 182, 0), 0.14)

    tile = render_icon(180)
    img.paste(tile, (64, 60), tile)

    d = ImageDraw.Draw(img)
    d.text((284, 116), "Rind", font=font(SF, 76, "Bold"), fill=(245, 245, 247), anchor="lm")
    tagline_f = ImageFont.truetype(tagline_font, 30)
    d.text((288, 186), tagline, font=tagline_f, fill=(166, 166, 174), anchor="lm")
    d.text((289, 236), subtitle, font=font(SF_MONO, 20, "Regular"), fill=(117, 117, 125), anchor="lm")
    img.save(out)


if __name__ == "__main__":
    main()