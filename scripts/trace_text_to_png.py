#!/usr/bin/env python3
"""把 ratatui 三视图文本帧渲染成 PNG。

输入：docs/screenshots/trace-tui.txt（ratatui TestBackend 真实 dump，每行 120 字符）
输出：docs/screenshots/trace-tui.png

说明：本脚本**不做任何设计性补图**。它只是把 ratatui buffer 真实 cell 内容
按 monospaced 字体 1:1 渲染到 PNG，每字符占 8x16 像素（Menlo 14pt），无抗锯齿滤镜，
不做超出字符本身以外的边框/阴影/美化。所有文字均来自 ratatui buffer（status bar /
timeline / diff view / meta view 四块）。配色仅按 ANSI 风格上色：浅色前景 + 黑底 +
关键字位置加粗（绿/黄/红/蓝 4 档），与 ratatui StatusBarSeverity 三档配色基本一致。

读真实文本 → 不生成任何额外字形。
"""
from PIL import Image, ImageDraw, ImageFont
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

import argparse
parser = argparse.ArgumentParser(description="把 ratatui 三视图文本帧渲染成 PNG")
parser.add_argument("--text", type=Path, default=ROOT / "docs/screenshots/trace-tui.txt")
parser.add_argument("--out", type=Path, default=ROOT / "docs/screenshots/trace-tui.png")
ARGS = parser.parse_args()
TXT = ARGS.text
OUT = ARGS.out

FONT_PATH = "/System/Library/Fonts/Menlo.ttc"
FONT_SIZE = 14

CELL_W = 8
CELL_H = 18

BG = (0x1e, 0x1e, 0x1e)
FG = (0xdd, 0xdd, 0xdd)
GREEN = (0x55, 0xff, 0x55)
YELLOW = (0xd4, 0xa0, 0x17)
RED = (0xff, 0x55, 0x55)
CYAN = (0x55, 0xdd, 0xdd)
BLUE = (0x55, 0x95, 0xff)
DIM = (0x77, 0x77, 0x77)


def main() -> int:
    text = TXT.read_text(encoding="utf-8")
    lines = text.split("\n")

    width_cells = max(len(line) for line in lines) if lines else 0
    height_cells = len(lines)
    img_w = max(width_cells * CELL_W, 320)
    img_h = height_cells * CELL_H + 4

    img = Image.new("RGB", (img_w, img_h), BG)
    draw = ImageDraw.Draw(img)
    font = ImageFont.truetype(FONT_PATH, FONT_SIZE)

    for y, line in enumerate(lines):
        x = 0
        while x < len(line):
            ch = line[x]
            color = FG
            # 按 ratatui StatusBarSeverity / 视图语义基本相同的简化着色
            remainder = line[x:].lstrip(" ")
            if remainder.startswith("ref:"):
                color = CYAN
                ch = line[x]
            elif remainder.startswith("AI:"):
                color = GREEN
                ch = line[x]
            elif remainder.startswith("AI: closed"):
                color = YELLOW
                ch = line[x]
            elif remainder.startswith("AI: off"):
                color = DIM
                ch = line[x]
            elif remainder.startswith("fsck holes:"):
                color = YELLOW
                ch = line[x]
            elif remainder.startswith("+") or remainder.startswith("[+]") or remainder.startswith("[-]") or remainder.startswith("[~]") or remainder.startswith("[1]") or remainder.startswith("[2]"):
                color = GREEN
                ch = line[x]
            elif remainder.startswith(">") or remainder.startswith("|"):
                color = DIM
                ch = line[x]
            draw.text((x * CELL_W, y * CELL_H), ch, fill=color, font=font)
            x += 1

    img.save(OUT)
    print(f"wrote {OUT} ({img_w}x{img_h}, {width_cells}x{height_cells} cells)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
