#!/usr/bin/env python3
"""从 256px 母版重新生成 assets/periphhub.ico。

为什么需要它（而不是让设计工具直接导出）：
  1. 提供常见 DPI 对应的尺寸，减少 Windows 再次缩放。100/125/150/
     175/200% 缩放下桌面「中图标」分别是 48/60/72/84/96，「大图标」是
     96/120/144/168/192。
  2. 母版最外圈是 12px、16% 不透明的**白色**描边。缩小时这圈白会顺着 alpha
     混进轮廓内侧，就是肉眼看到的白边/锯齿。这里先把描边去掉，只保留
     黑色圆角方块的轮廓（当作超采样源），再向下采样。
  3. 采样必须在**预乘 alpha** 空间做，否则半透明像素的颜色会污染邻居。

16–32 保留像素对齐的原生字形，确保小图标可读；40px 及以上统一从母版重采样，
让桌面图标的字形曲线与圆角都有连续的抗锯齿过渡。

母版与产物必须分开存：`--src` 是 assets/periphhub-master.ico（只含 256 母版
和 16–48 的手绘条目），`--out` 是发布用的 assets/periphhub.ico。母版一旦被
产物覆盖就回不来了——白描边和四周透明边会永久丢失，再跑一次只能从"已经削过
描边、已经抗锯齿过"的图上再削，轮廓会变实、变形。脚本因此拒绝 src 与 out
指向同一个文件。

用法：python scripts/make-ico.py [--src assets/periphhub-master.ico]
                                 [--out assets/periphhub.ico]
依赖：仅标准库。
"""

from __future__ import annotations

import argparse
import math
import struct
import sys
import zlib
from pathlib import Path

# Windows 在常见 DPI 缩放 + 图标档位组合下会请求的尺寸。
# 16/20/24/28/32 = 小图标 ×100~175%；48/60/72/84/96 = 中图标；
# 96/120/144/168/192 = 大图标；256 = 超大图标。
SIZES = [16, 20, 24, 28, 32, 40, 48, 56, 60, 64, 72, 84, 96, 120, 128, 144, 168, 192, 256]

# 最小尺寸保留像素对齐的字形；桌面尺寸必须经过重采样以平滑曲线。
KEEP = {16, 20, 24, 32}

# 母版外圈白色描边的 alpha 平台值（实测 40）。低于它的像素只属于描边。
RIM_ALPHA = 40
BODY = (0x10, 0x10, 0x10)


# ---------------------------------------------------------------- ICO 读取


def decode_png(data: bytes) -> tuple[int, int, list[tuple[int, int, int, int]]]:
    """只解 8bit RGBA、filter 0/1/2/3/4 的 PNG（够本脚本自己写出的格式用）。"""
    w, h = struct.unpack(">II", data[16:24])
    depth, color = data[24], data[25]
    if depth != 8 or color != 6:
        raise ValueError("只支持 8bit RGBA 的 PNG 条目")
    i, idat = 8, b""
    while i < len(data):
        ln = struct.unpack(">I", data[i : i + 4])[0]
        tag = data[i + 4 : i + 8]
        if tag == b"IDAT":
            idat += data[i + 8 : i + 8 + ln]
        i += 12 + ln
    raw = zlib.decompress(idat)
    stride = w * 4
    out: list[tuple[int, int, int, int]] = []
    prev = bytearray(stride)
    for y in range(h):
        base = y * (stride + 1)
        f = raw[base]
        line = bytearray(raw[base + 1 : base + 1 + stride])
        if f == 1:
            for x in range(4, stride):
                line[x] = (line[x] + line[x - 4]) & 0xFF
        elif f == 2:
            for x in range(stride):
                line[x] = (line[x] + prev[x]) & 0xFF
        elif f == 3:
            for x in range(stride):
                a = line[x - 4] if x >= 4 else 0
                line[x] = (line[x] + ((a + prev[x]) >> 1)) & 0xFF
        elif f == 4:
            for x in range(stride):
                a = line[x - 4] if x >= 4 else 0
                b = prev[x]
                c = prev[x - 4] if x >= 4 else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[x] = (line[x] + pr) & 0xFF
        for x in range(w):
            o = x * 4
            out.append((line[o], line[o + 1], line[o + 2], line[o + 3]))
        prev = line
    return w, h, out


def read_ico(data: bytes) -> dict[int, tuple[int, int, list[tuple[int, int, int, int]]]]:
    """返回 {边长: (w, h, [(r,g,b,a)...])}，支持 32bpp DIB 与 PNG 条目。"""
    count = struct.unpack_from("<H", data, 4)[0]
    out: dict[int, tuple[int, int, list[tuple[int, int, int, int]]]] = {}
    for i in range(count):
        w, h, _cc, _r, _planes, bpp, size, off = struct.unpack_from("<BBBBHHII", data, 6 + 16 * i)
        w = w or 256
        h = h or 256
        blob = data[off : off + size]
        if blob[:8] == b"\x89PNG\r\n\x1a\n":
            pw, ph, pixels = decode_png(blob)
            out[pw] = (pw, ph, pixels)
            continue
        if bpp != 32:
            continue
        if struct.unpack_from("<I", blob, 0)[0] != 40:
            continue
        px = blob[40 : 40 + w * h * 4]
        pixels: list[tuple[int, int, int, int]] = []
        for y in range(h - 1, -1, -1):  # DIB 自下而上
            row = y * w * 4
            for x in range(w):
                o = row + x * 4
                b_, g_, r_, a_ = px[o : o + 4]
                pixels.append((r_, g_, b_, a_))
        out[w] = (w, h, pixels)
    return out


# ---------------------------------------------------------------- 采样


def weight_table(src: int, dst: int) -> list[list[tuple[int, float]]]:
    """面积平均（box）的采样权重。

    刻意不用 Lanczos：它的负瓣会在 1% 覆盖率处过冲，反预乘之后变成浅灰描边
    （实测 60px 上出现 RGB 210 / alpha 52 的一整圈），那正是放大后发毛的来源。
    母版 256px 对最小 16px 有 16 倍超采样，面积平均本身就等价于高质量超采样，
    不会因此变糊；而且全程权重非负，alpha 和颜色都不会振铃。
    """
    scale = src / dst
    table: list[list[tuple[int, float]]] = []
    for i in range(dst):
        lo = i * scale
        hi = lo + scale
        taps = []
        total = 0.0
        # 上限必须夹到 src：浮点累加会让最后一格的 hi 略大于 src（232.0000001），
        # ceil 之后就会多取一格而越界。
        for j in range(int(math.floor(lo)), min(src, int(math.ceil(hi)))):
            overlap = min(hi, j + 1) - max(lo, j)
            if overlap > 0.0:
                taps.append((j, overlap))
                total += overlap
        if total == 0.0:
            taps = [(min(src - 1, int(lo)), 1.0)]
            total = 1.0
        table.append([(j, w / total) for j, w in taps])
    return table


def resample(src_w: int, src_h: int, px: list[tuple[float, float, float, float]], dst: int) -> list[tuple[float, float, float, float]]:
    """预乘空间里的分离式重采样。px 已是 (r*a, g*a, b*a, a)。"""
    if src_w == dst and src_h == dst:
        return px

    wx = weight_table(src_w, dst)
    # 横向
    mid: list[tuple[float, float, float, float]] = []
    for y in range(src_h):
        row = y * src_w
        for taps in wx:
            r = g = b = a = 0.0
            for j, w in taps:
                p = px[row + j]
                r += p[0] * w
                g += p[1] * w
                b += p[2] * w
                a += p[3] * w
            mid.append((r, g, b, a))
    # 纵向
    wy = weight_table(src_h, dst)
    out: list[tuple[float, float, float, float]] = []
    for taps in wy:
        for x in range(dst):
            r = g = b = a = 0.0
            for j, w in taps:
                p = mid[j * dst + x]
                r += p[0] * w
                g += p[1] * w
                b += p[2] * w
                a += p[3] * w
            out.append((r, g, b, a))
    return out


def unpremultiply(px: list[tuple[float, float, float, float]]) -> list[tuple[int, int, int, int]]:
    """反预乘。

    这套图标只有两种颜色（主体 #101010、字形 #F5F5F5），而且字形完全在主体
    内部——所以任何 0<alpha<255 的像素只可能是**轮廓边缘**。这类像素的 RGB
    一律设成主体色：既避免反预乘把极小的分母放大成浅灰，也保证 Windows 再
    缩放这个尺寸时不会有白边渗进边缘。
    """
    out = []
    for r, g, b, a in px:
        a255 = max(0, min(255, int(a + 0.5)))
        if a255 == 0:
            out.append((*BODY, 0))
        elif a255 == 255:
            out.append(
                (
                    max(0, min(255, int(r + 0.5))),
                    max(0, min(255, int(g + 0.5))),
                    max(0, min(255, int(b + 0.5))),
                    255,
                )
            )
        else:
            out.append((*BODY, a255))
    return out


# ---------------------------------------------------------------- 写入


def dib_entry(w: int, h: int, px: list[tuple[int, int, int, int]]) -> bytes:
    """32bpp BITMAPINFOHEADER + 全零 AND mask，自下而上。"""
    header = struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    body = bytearray()
    for y in range(h - 1, -1, -1):
        for x in range(w):
            r, g, b, a = px[y * w + x]
            body += bytes((b, g, r, a))
    mask_row = ((w + 31) // 32) * 4
    return header + bytes(body) + b"\x00" * (mask_row * h)


def png_entry(w: int, h: int, px: list[tuple[int, int, int, int]]) -> bytes:
    """编码 RGBA PNG，供大尺寸 ICO 条目使用。"""
    raw = bytearray()
    for y in range(h):
        raw.append(0)  # filter: none
        for x in range(w):
            r, g, b, a = px[y * w + x]
            raw += bytes((r, g, b, a))

    def chunk(tag: bytes, payload: bytes) -> bytes:
        """生成带 CRC 校验的 PNG 数据块。"""
        return (
            struct.pack(">I", len(payload))
            + tag
            + payload
            + struct.pack(">I", zlib.crc32(tag + payload) & 0xFFFFFFFF)
        )

    ihdr = struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )


def main() -> int:
    """校验只读母版并生成各 DPI 所需的图标条目。"""
    ap = argparse.ArgumentParser()
    ap.add_argument("--src", default="assets/periphhub-master.ico", help="母版（只读，含 256 与手绘小尺寸）")
    ap.add_argument("--out", default="assets/periphhub.ico", help="生成的图标")
    args = ap.parse_args()

    src_path = Path(args.src)
    out_path = Path(args.out)
    # 母版被产物覆盖是不可逆的：母版里的白描边和透明边是唯一一份。
    if src_path.exists() and out_path.exists() and src_path.resolve() == out_path.resolve():
        print(
            f"--src 与 --out 都是 {src_path}：这会把母版覆盖掉，之后再也生成不出干净的图标。\n"
            "母版请单独存（默认 assets/periphhub-master.ico）。",
            file=sys.stderr,
        )
        return 1
    if not src_path.exists():
        print(f"找不到母版：{src_path}", file=sys.stderr)
        return 1

    entries = read_ico(src_path.read_bytes())
    if 256 not in entries:
        print("母版里没有 256px 条目", file=sys.stderr)
        return 1
    # 手绘小尺寸不是本脚本能重算出来的：母版缺了就说明它不是原始母版
    # （典型情况是拿产物当母版），此时静默重采样会白掉一档锐度。
    missing = sorted(KEEP - set(entries))
    if missing:
        print(
            f"母版缺少手绘尺寸 {missing}：只能从原始母版重新生成，"
            "不能拿已经生成的产物当母版。",
            file=sys.stderr,
        )
        return 1
    # 产物是裁到贴边的，真母版四周留了透明边（最外圈最多只剩描边那点 alpha）。
    # 用这个结构性差别把"拿产物当母版"挡在写盘之前——否则白描边和抗锯齿信息
    # 会被再削一遍，而且削完看不出来，只有下次生成时才发现轮廓变实了。
    _w, _h, _px = entries[256]
    border = (
        [_px[i] for i in range(_w)]
        + [_px[(_h - 1) * _w + i] for i in range(_w)]
        + [_px[y * _w] for y in range(_h)]
        + [_px[y * _w + _w - 1] for y in range(_h)]
    )
    if any(p[3] > RIM_ALPHA * 2 for p in border):
        print(
            "母版 256 条目贴着边框（四周没有透明边）：这是脚本生成的产物，不是原始母版。\n"
            "请使用 assets/periphhub-master.ico。",
            file=sys.stderr,
        )
        return 1

    # 1) 母版 → 轮廓：丢掉 16% 白描边，只留黑色圆角方块本身。
    #    这里直接用 alpha 阈值取形状：向下采样等价于超采样，边缘会自然得到
    #    多级抗锯齿，比在低分辨率上手工涂更干净。
    _, _, master = entries[256]
    silhouette: list[tuple[int, int, int, int]] = []
    for r, g, b, a in master:
        if a > RIM_ALPHA * 2:  # 属于方块本体（内部或边缘过渡像素）
            # 半透明母版像素混有白描边；字形位于不透明主体内，只保留它的原色。
            color = (r, g, b) if a == 255 else BODY
            silhouette.append((*color, 255))
        else:  # 只剩描边或完全透明
            silhouette.append((*BODY, 0))

    # 裁掉母版的透明边，让重采样尺寸与原生小图标保持一致的主体大小。
    xs = [i % 256 for i, p in enumerate(silhouette) if p[3]]
    ys = [i // 256 for i, p in enumerate(silhouette) if p[3]]
    x0, x1 = min(xs), max(xs) + 1
    y0, y1 = min(ys), max(ys) + 1
    side = max(x1 - x0, y1 - y0)
    left = min(256 - side, max(0, (x0 + x1 - side) // 2))
    top = min(256 - side, max(0, (y0 + y1 - side) // 2))
    silhouette = [
        silhouette[(top + y) * 256 + (left + x)] for y in range(side) for x in range(side)
    ]

    premul = [(r * a / 255.0, g * a / 255.0, b * a / 255.0, float(a)) for r, g, b, a in silhouette]

    # 2) 生成每一档
    built: dict[int, list[tuple[int, int, int, int]]] = {}
    for size in SIZES:
        if size in KEEP:  # 手绘条目原样带过去（上面已校验母版里都有）
            built[size] = entries[size][2]
            continue
        built[size] = unpremultiply(resample(side, side, premul, size))

    # 3) 打包：≥144 用 PNG 控制体积，其余用 DIB
    blobs: list[tuple[int, bytes]] = []
    for size in SIZES:
        px = built[size]
        blobs.append((size, png_entry(size, size, px) if size >= 144 else dib_entry(size, size, px)))

    offset = 6 + 16 * len(blobs)
    directory = b""
    payload = b""
    for size, blob in blobs:
        dim = 0 if size >= 256 else size
        directory += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(blob), offset)
        payload += blob
        offset += len(blob)

    out = struct.pack("<HHH", 0, 1, len(blobs)) + directory + payload
    out_path.write_bytes(out)

    print(f"{args.out}: {len(blobs)} 档, {len(out) / 1024:.0f} KB")
    print("  " + ", ".join(f"{s}px" for s, _ in blobs))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
