"""纯标准库 PNG 掩码写盘（给 `/op/inpaint` 用）。

为什么不用 Pillow：官方包**不背第三方依赖**（`requirements.txt` 为空，包内 venv 免装），
而写一张 8bit 灰度 PNG 只需要 `zlib` + `struct`——`/detect/ocr` 给的文字框
放大成实心白块，就是一张合法掩码。读图仍全部走网关（`/io/read`、`/detect/u2net`），
插件不解析大图。
"""
from __future__ import annotations

import struct
import zlib

MARGIN_RATIO = 0.012      # 文字框外扩（按宽/高比例），吃掉抗锯齿边缘


def _chunk(tag, payload):
    return (struct.pack(">I", len(payload)) + tag + payload
            + struct.pack(">I", zlib.crc32(tag + payload) & 0xFFFFFFFF))


def write_mask_png(path, width, height, boxes, *, margin_ratio=MARGIN_RATIO):
    """把 `[[x1,y1,x2,y2], ...]` 画成白块掩码（灰度为 0 的底）。

    参数越界一律夹到图像范围内；空 boxes 也写一张全黑掩码（调用方据此跳过 inpaint）。
    """
    width, height = int(width), int(height)
    if width <= 0 or height <= 0:
        raise ValueError("mask 尺寸必须为正：%dx%d" % (width, height))
    mx = max(1, int(round(width * float(margin_ratio))))
    my = max(1, int(round(height * float(margin_ratio))))

    stride = width + 1                      # 每行前置一个 filter 字节（0 = None）
    raw = bytearray(stride * height)
    for box in boxes or []:
        x1, y1, x2, y2 = [int(round(float(v))) for v in box[:4]]
        x1, x2 = sorted((x1, x2))
        y1, y2 = sorted((y1, y2))
        x1 = max(0, x1 - mx)
        y1 = max(0, y1 - my)
        x2 = min(width - 1, x2 + mx)
        y2 = min(height - 1, y2 + my)
        if x2 < x1 or y2 < y1:
            continue
        span = x2 - x1 + 1
        fill = b"\xff" * span
        for y in range(y1, y2 + 1):
            start = y * stride + 1 + x1
            raw[start:start + span] = fill

    blob = b"\x89PNG\r\n\x1a\n"
    blob += _chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 0, 0, 0, 0))
    blob += _chunk(b"IDAT", zlib.compress(bytes(raw), 6))
    blob += _chunk(b"IEND", b"")
    with open(path, "wb") as fh:
        fh.write(blob)
    return path


def read_mask_png(path):
    """只解本模块写出的这种掩码（8bit 灰度、filter=None、非隔行），给单测用。

    通用解码不在插件范围内——真读图走网关。
    """
    with open(path, "rb") as fh:
        blob = fh.read()
    if not blob.startswith(b"\x89PNG\r\n\x1a\n"):
        raise ValueError("不是 PNG")
    pos = 8
    width = height = None
    idat = bytearray()
    while pos < len(blob):
        length = struct.unpack(">I", blob[pos:pos + 4])[0]
        tag = blob[pos + 4:pos + 8]
        payload = blob[pos + 8:pos + 8 + length]
        pos += 12 + length
        if tag == b"IHDR":
            width, height, depth, color = struct.unpack(">IIBB", payload[:10])
            if depth != 8 or color != 0:
                raise ValueError("只支持 8bit 灰度掩码")
            if payload[12] != 0:
                raise ValueError("不支持隔行 PNG")
        elif tag == b"IDAT":
            idat += payload
        elif tag == b"IEND":
            break
    if width is None or height is None:
        raise ValueError("缺 IHDR")
    raw = zlib.decompress(bytes(idat))

    stride = width + 1
    rows = []
    for y in range(height):
        line = raw[y * stride:(y + 1) * stride]
        if line[0] != 0:
            raise ValueError("掩码写了非 None filter，读不了")
        rows.append(line[1:])
    return width, height, rows
