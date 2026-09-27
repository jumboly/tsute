#!/usr/bin/env python3
"""アイコン PNG を依存ライブラリなしで生成する。

なぜ: 画像編集ツールや追加パッケージなしで CI/ローカルとも再現可能にするため。
デザインは暫定（円＋「つて」を表す2点を結ぶ線）で、後で差し替え可能。
"""
import math, struct, zlib, sys, os

def write_png(path, w, h, pixel):
    rows = []
    for y in range(h):
        row = bytearray(b"\x00")
        for x in range(w):
            row += bytes(pixel(x, y))
        rows.append(bytes(row))
    def chunk(t, d):
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)
    data = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
    data += chunk(b"IDAT", zlib.compress(b"".join(rows), 9)) + chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(data)

def dist_seg(px, py, ax, ay, bx, by):
    dx, dy = bx - ax, by - ay
    t = max(0, min(1, ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)))
    return math.hypot(px - (ax + t * dx), py - (ay + t * dy))

def glyph(s, x, y):
    """単位座標で2つの点とそれを結ぶ弧状の線（端点間の“つて”）。戻り値は被覆率 0..1"""
    u, v = x / s, y / s
    a, b = (0.28, 0.66), (0.72, 0.34)
    d = min(math.hypot(u - a[0], v - a[1]) - 0.11, math.hypot(u - b[0], v - b[1]) - 0.11,
            dist_seg(u, v, a[0], a[1], b[0], b[1]) - 0.045)
    return max(0.0, min(1.0, 0.5 - d * s))

def app_icon(size):
    c = (size - 1) / 2
    def px(x, y):
        r = math.hypot(x - c, y - c)
        bg = max(0.0, min(1.0, size * 0.45 - r + 0.5))
        g = glyph(size, x, y)
        col = [int(28 * (1 - g) + 255 * g), int(100 * (1 - g) + 255 * g), int(190 * (1 - g) + 255 * g)]
        return (*col, int(255 * bg))
    return px

def tray_icon(size):
    # macOS のテンプレート画像: 黒+アルファのみ（OSがダーク/ライトで自動反転するため）
    def px(x, y):
        return (0, 0, 0, int(255 * glyph(size, x, y)))
    return px

out = sys.argv[1] if len(sys.argv) > 1 else "apps/desktop/icons"
os.makedirs(out, exist_ok=True)
write_png(f"{out}/icon.png", 512, 512, app_icon(512))
write_png(f"{out}/tray.png", 44, 44, tray_icon(44))
print("ok")
