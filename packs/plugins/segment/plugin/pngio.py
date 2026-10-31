#!/usr/bin/env python3
"""最小 PNG 编解码（纯标准库）—— 抠图包的像素出入口。

为什么包里自己写 PNG：本包零第三方依赖（`requirements.txt` 只有注释），而
`plugin-design-and-ui.md` §2 把「抠图/换背景」列为**需独立引擎**的插件，宿主的
运行时不会替包 `pip install`（`ensure_pack_venv` 只建 venv 不装依赖，见
`crates/app-web/src/compliance_api.rs::ensure_pack_venv`）。真实需求又必须碰像素，
所以只支持**我们真正用到的子集**，越界一律显式报错（不猜、不静默）：

解码（宿主产物）：8bit、非隔行，颜色类型 0/2/6（灰度/RGB/RGBA）。
    宿主 `core-media` 的 `PngEncoder` 会用自适应 filter（0..4），所以五种 filter
    都必须实现 —— 否则换个图就解不开。
编码（本包产物）：8bit RGB / RGBA，filter 固定 None（可复现：同输入同字节）。

确定性口径（N14 要求「同参数重跑 sha256 一致」）：
    - 编码不写 tEXt/tIME，zlib 固定 level=6，chunk 顺序固定；
    - 解码/编码都不依赖系统时间、locale 或随机数。
"""
from __future__ import annotations

import struct
import zlib

PNG_SIG = b"\x89PNG\r\n\x1a\n"


class PngError(Exception):
    """像素格式不支持 / 数据损坏（插件一律映射成 -32000，不冒充成功）。"""


def _chunks(blob):
    if not blob.startswith(PNG_SIG):
        raise PngError("不是 PNG（缺签名）")
    pos = 8
    end = len(blob)
    while pos + 8 <= end:
        (length,) = struct.unpack(">I", blob[pos:pos + 4])
        tag = blob[pos + 4:pos + 8]
        payload = blob[pos + 8:pos + 8 + length]
        if len(payload) != length:
            raise PngError("PNG chunk 被截断：%r" % (tag,))
        yield tag, payload
        pos += 12 + length


def decode_path(path):
    """读文件 → `(width, height, channels, raw)`；channels ∈ {1,3,4}。"""
    with open(path, "rb") as fh:
        return decode_bytes(fh.read())


def decode_bytes(blob):
    width = height = None
    depth = color = None
    interlace = 0
    idat = []
    for tag, payload in _chunks(blob):
        if tag == b"IHDR":
            width, height, depth, color, _comp, _filt, interlace = struct.unpack(
                ">IIBBBBB", payload[:13])
        elif tag == b"IDAT":
            idat.append(payload)
        elif tag == b"IEND":
            break
    if width is None:
        raise PngError("PNG 缺 IHDR")
    if depth != 8:
        raise PngError("只支持 8bit PNG（实得 %r）" % (depth,))
    if interlace != 0:
        raise PngError("不支持隔行 PNG")
    channels = {0: 1, 2: 3, 6: 4}.get(color)
    if channels is None:
        raise PngError("只支持灰度/RGB/RGBA PNG（颜色类型 0/2/6，实得 %r）" % (color,))
    if not idat:
        raise PngError("PNG 缺 IDAT")
    try:
        raw = zlib.decompress(b"".join(idat))
    except zlib.error as exc:
        raise PngError("PNG IDAT 解压失败：%s" % (exc,))
    return width, height, channels, _unfilter(raw, width, height, channels)


def _unfilter(raw, width, height, channels):
    """逐行去 filter（0 None / 1 Sub / 2 Up / 3 Average / 4 Paeth）。"""
    stride = width * channels
    expect = (stride + 1) * height
    if len(raw) < expect:
        raise PngError("PNG 像素数据不足（%d < %d）" % (len(raw), expect))
    out = bytearray(stride * height)
    prev = bytearray(stride)
    bpp = channels
    pos = 0
    for y in range(height):
        ftype = raw[pos]
        line = raw[pos + 1:pos + 1 + stride]
        pos += stride + 1
        cur = bytearray(line)
        if ftype == 0:
            pass
        elif ftype == 1:
            for i in range(bpp, stride):
                cur[i] = (cur[i] + cur[i - bpp]) & 0xFF
        elif ftype == 2:
            for i in range(stride):
                cur[i] = (cur[i] + prev[i]) & 0xFF
        elif ftype == 3:
            for i in range(stride):
                left = cur[i - bpp] if i >= bpp else 0
                cur[i] = (cur[i] + ((left + prev[i]) >> 1)) & 0xFF
        elif ftype == 4:
            for i in range(stride):
                if i >= bpp:
                    a = cur[i - bpp]
                    c = prev[i - bpp]
                else:
                    a = c = 0
                b = prev[i]
                p = a + b - c
                pa = p - a if p > a else a - p
                pb = p - b if p > b else b - p
                pc = p - c if p > c else c - p
                if pa <= pb and pa <= pc:
                    cur[i] = (cur[i] + a) & 0xFF
                elif pb <= pc:
                    cur[i] = (cur[i] + b) & 0xFF
                else:
                    cur[i] = (cur[i] + c) & 0xFF
        else:
            raise PngError("未知 PNG filter 类型 %r" % (ftype,))
        out[y * stride:(y + 1) * stride] = cur
        prev = cur
    return out


def _chunk(tag, payload):
    return (struct.pack(">I", len(payload)) + tag + payload
            + struct.pack(">I", zlib.crc32(tag + payload) & 0xFFFFFFFF))


def encode(width, height, channels, raw, level=6):
    """`raw` 长度必须是 width*height*channels → PNG 字节（filter 固定 None）。"""
    if channels not in (1, 3, 4):
        raise PngError("编码只支持 1/3/4 通道（实得 %r）" % (channels,))
    stride = width * channels
    if len(raw) != stride * height:
        raise PngError("像素缓冲 %d 与 %dx%d×%d 不符" % (len(raw), width, height, channels))
    color = {1: 0, 3: 2, 4: 6}[channels]
    rows = bytearray((stride + 1) * height)
    for y in range(height):
        dst = y * (stride + 1)
        rows[dst] = 0                      # filter None：可复现，且宿主解码器认
        rows[dst + 1:dst + 1 + stride] = raw[y * stride:(y + 1) * stride]
    ihdr = struct.pack(">IIBBBBB", width, height, 8, color, 0, 0, 0)
    return (PNG_SIG
            + _chunk(b"IHDR", ihdr)
            + _chunk(b"IDAT", zlib.compress(bytes(rows), level))
            + _chunk(b"IEND", b""))


def write_png(path, width, height, channels, raw):
    blob = encode(width, height, channels, raw)
    with open(path, "wb") as fh:
        fh.write(blob)
    return blob
