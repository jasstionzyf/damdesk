"""房产包 —— 纯标准库图像解码（只解出 Rec.709 luma 平面）。

为什么包内自带解码器：
    宿主能力网关 v1.0 的 10 个端点里**没有像素统计**（只有 io.read 的元数据、
    u2net 的主体掩码、OCR 文本、几何/generative 操作、VL 判定）。而房产包的
    F1 清晰度（拉普拉斯方差）与 F2 曝光（死区占比）按旧引擎口径是 **classic
    像素统计**，必须拿到 luma 才能算。

    三条备选都被否决过：
      - 改宿主加 classic 端点 → 违反「新增行业插件时宿主零改动」这条架构承诺；
      - 改走 VL 判清晰度/曝光 → 丢掉确定性（同一张图两次判定可能不同），
        而这两条恰恰是房产包里唯一能给硬结论的规则；
      - 在 requirements.txt 里加 Pillow/numpy → 包内 venv 要背 C 扩展体积与
        CVE，且要按平台各带一份 wheel（官方电商包刻意保持零依赖的同一理由）。

    所以这里用标准库自己解：PNG（zlib 解压 + 反滤波）与 baseline JPEG
    （Huffman + 稀疏 IDCT + YCbCr→Rec.709 luma）。**只产 luma 平面**，
    不产 RGB、不写文件、不出网。

口径与宿主 `engine-classic` 对齐：
    luma = (2126·R + 7152·G + 722·B) / 10000（整型截断，见
    `crates/engine-classic/src/detectors.rs::luma_from_rgb`）。
    等价地，对 JPEG（JFIF full-range YCbCr，Rec.601 逆变换）可写成
    luma = Y − 0.21269·(Cr−128) − 0.11819·(Cb−128)（本文件用这个式子，
    省掉逐像素 RGB 往返；与逐通道截断版差 ≤1 级）。
    推导：R=Y+1.402(Cr−128)、B=Y+1.772(Cb−128)、
    G=Y−0.344136(Cb−128)−0.714136(Cr−128)，代入
    (2126R+7152G+722B)/10000；Cr 系数 = (2126·1.402 − 7152·0.714136)/10000。

不支持（明确报 `DecodeError`，判定侧落 undetermined 转人工，绝不猜）：
    - progressive / arithmetic JPEG（SOF2/SOF9~11）、lossless（SOF3）
    - PNG 交错（Adam7）、位深 1/2/4（调色板低深）
    解码规模上限 `MAX_PIXELS`（默认 2400 万像素）防止单图拖爆宿主超时预算。
"""
from __future__ import annotations

import struct
import zlib

MAX_PIXELS = 24_000_000


class DecodeError(Exception):
    """解码失败（格式不支持 / 数据损坏）。判定侧一律转 undetermined。"""


# ---------------------------------------------------------------------------
# 入口
# ---------------------------------------------------------------------------
def decode_luma(path):
    """解出 `(width, height, luma)`；luma 是 bytearray（每像素 1 字节）。"""
    return _decode(path, dc_only=False)


def decode_dc_luma(path):
    """只解 DC 系数（1/8 缩放）的 luma 平面 —— 便宜，够算 pHash。

    返回 `(width, height, luma)`，尺寸是 ceil(原尺寸/8) 量级（JPEG 走 DCT 块栅格，
    PNG 走 8×8 面积平均）。用于 F3 的同组成员比对：一组几十张图时 IPC/耗时
    只有全解码的几分之一。
    """
    return _decode(path, dc_only=True)


def _decode(path, dc_only):
    try:
        with open(path, "rb") as fh:
            data = fh.read()
    except OSError as exc:
        raise DecodeError("读取失败：%s" % (exc,))
    if not data:
        raise DecodeError("空文件")
    if data[:8] == b"\x89PNG\r\n\x1a\n":
        width, height, luma = _png_luma(data)
    elif data[:2] == b"\xff\xd8":
        return _jpeg_luma(data, dc_only=dc_only)
    else:
        raise DecodeError("未知图像格式（既不是 PNG 也不是 JPEG）")
    if not dc_only:
        return width, height, luma
    return _downscale_plane(luma, width, height, 8)


def _downscale_plane(plane, width, height, factor):
    """面积平均降采样（PNG 走这条；JPEG 的 DC 平面本来就是 1/8）。"""
    out_w = max(1, width // factor)
    out_h = max(1, height // factor)
    out = bytearray(out_w * out_h)
    for oy in range(out_h):
        y0 = oy * height // out_h
        y1 = max(y0 + 1, (oy + 1) * height // out_h)
        for ox in range(out_w):
            x0 = ox * width // out_w
            x1 = max(x0 + 1, (ox + 1) * width // out_w)
            total = 0
            count = 0
            for y in range(y0, y1):
                row = y * width
                for x in range(x0, x1):
                    total += plane[row + x]
                    count += 1
            out[oy * out_w + ox] = total // count if count else 0
    return out_w, out_h, out


# ---------------------------------------------------------------------------
# PNG
# ---------------------------------------------------------------------------
_PNG_CHANNELS = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}


def _png_luma(data):
    pos = 8
    width = height = 0
    depth = 8
    color = 6
    interlace = 0
    palette = b""
    idat = bytearray()
    while pos + 8 <= len(data):
        length, ctype = struct.unpack(">I4s", data[pos:pos + 8])
        body = data[pos + 8:pos + 8 + length]
        pos += 12 + length
        if ctype == b"IHDR":
            if len(body) < 13:
                raise DecodeError("PNG IHDR 长度不足")
            width, height, depth, color, comp, filt, interlace = struct.unpack(
                ">IIBBBBB", body[:13])
            if comp != 0 or filt != 0:
                raise DecodeError("PNG 压缩/滤波方式不支持")
            if interlace != 0:
                raise DecodeError("PNG 交错（Adam7）不支持")
        elif ctype == b"PLTE":
            palette = body
        elif ctype == b"IDAT":
            idat += body
        elif ctype == b"IEND":
            break
    if not width or not height:
        raise DecodeError("PNG 缺 IHDR")
    if width * height > MAX_PIXELS:
        raise DecodeError("图像过大：%dx%d" % (width, height))
    if color not in _PNG_CHANNELS:
        raise DecodeError("PNG color_type=%d 不支持" % color)
    if depth not in (8, 16):
        raise DecodeError("PNG 位深 %d 不支持（只支持 8/16）" % depth)
    channels = _PNG_CHANNELS[color]
    if color == 3 and not palette:
        raise DecodeError("PNG 调色板图缺 PLTE")
    sample_bytes = depth // 8
    bpp = max(1, channels * sample_bytes)
    stride = width * bpp
    try:
        raw = zlib.decompress(bytes(idat))
    except zlib.error as exc:
        raise DecodeError("PNG IDAT 解压失败：%s" % (exc,))
    if len(raw) < (stride + 1) * height:
        raise DecodeError("PNG 像素数据不足")

    luma = bytearray(width * height)
    prev = bytearray(stride)
    off = 0
    for y in range(height):
        ftype = raw[off]
        line = bytearray(raw[off + 1:off + 1 + stride])
        off += 1 + stride
        _png_unfilter(ftype, line, prev, bpp)
        prev = line
        row = y * width
        if color == 0:
            if depth == 8:
                luma[row:row + width] = line
            else:
                luma[row:row + width] = line[0::2]
        elif color == 4:
            if depth == 8:
                luma[row:row + width] = line[0::2]
            else:
                luma[row:row + width] = line[0::4]
        elif color == 2:
            step = 3 * sample_bytes
            for x in range(width):
                base = x * step
                luma[row + x] = _luma709(line[base], line[base + sample_bytes],
                                         line[base + 2 * sample_bytes])
        elif color == 6:
            step = 4 * sample_bytes
            for x in range(width):
                base = x * step
                luma[row + x] = _luma709(line[base], line[base + sample_bytes],
                                         line[base + 2 * sample_bytes])
        else:  # color == 3：调色板索引
            idx_step = sample_bytes
            for x in range(width):
                idx = line[x * idx_step]
                base = idx * 3
                if base + 2 >= len(palette):
                    raise DecodeError("PNG 调色板索引越界")
                luma[row + x] = _luma709(palette[base], palette[base + 1],
                                         palette[base + 2])
    return width, height, luma


def _png_unfilter(ftype, line, prev, bpp):
    n = len(line)
    if ftype == 0:
        return
    if ftype == 1:
        for i in range(bpp, n):
            line[i] = (line[i] + line[i - bpp]) & 0xFF
        return
    if ftype == 2:
        for i in range(n):
            line[i] = (line[i] + prev[i]) & 0xFF
        return
    if ftype == 3:
        for i in range(n):
            left = line[i - bpp] if i >= bpp else 0
            line[i] = (line[i] + ((left + prev[i]) >> 1)) & 0xFF
        return
    if ftype == 4:
        for i in range(n):
            if i >= bpp:
                a = line[i - bpp]
                c = prev[i - bpp]
            else:
                a = 0
                c = 0
            b = prev[i]
            p = a + b - c
            pa = abs(p - a)
            pb = abs(p - b)
            pc = abs(p - c)
            if pa <= pb and pa <= pc:
                pred = a
            elif pb <= pc:
                pred = b
            else:
                pred = c
            line[i] = (line[i] + pred) & 0xFF
        return
    raise DecodeError("PNG 行滤波类型 %d 非法" % ftype)


def _luma709(r, g, b):
    value = (2126 * r + 7152 * g + 722 * b) // 10000
    return 255 if value > 255 else value


# ---------------------------------------------------------------------------
# JPEG（baseline / extended sequential）
# ---------------------------------------------------------------------------
_ZIGZAG = (
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5,
    12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
)

# 1-D 8 点 IDCT 基：_COS[u][x] = C(u)·cos((2x+1)uπ/16)，C(0)=1/√2
_COS = [
    [0.7071067811865476 * _c for _c in (
        1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0)]
]
import math as _math  # noqa: E402  （放这里只为让 _COS 表紧邻语义）

_COS = []
for _u in range(8):
    _cu = 0.7071067811865476 if _u == 0 else 1.0
    _COS.append([_cu * _math.cos((2 * _x + 1) * _u * _math.pi / 16.0)
                 for _x in range(8)])


class _Bits:
    """熵段比特流（处理 0xFF00 填充；遇标记即视为段结束，补零）。"""

    __slots__ = ("d", "i", "buf", "n", "eof")

    def __init__(self, data, pos):
        self.d = data
        self.i = pos
        self.buf = 0
        self.n = 0
        self.eof = False

    def _fill(self):
        d = self.d
        n = self.n
        buf = self.buf
        i = self.i
        size = len(d)
        while n <= 24:
            if i >= size:
                self.eof = True
                buf = (buf << 8) & 0xFFFFFFFFFFFF
                n += 8
                continue
            b = d[i]
            if b == 0xFF:
                nxt = d[i + 1] if i + 1 < size else 0
                if nxt == 0x00:
                    i += 2
                else:                       # 真实标记：熵段到此为止
                    self.eof = True
                    buf = (buf << 8) & 0xFFFFFFFFFFFF
                    n += 8
                    continue
            else:
                i += 1
            buf = ((buf << 8) | b) & 0xFFFFFFFFFFFF
            n += 8
        self.buf = buf
        self.n = n
        self.i = i

    def peek(self, k):
        if self.n < k:
            self._fill()
        return (self.buf >> (self.n - k)) & ((1 << k) - 1)

    def skip(self, k):
        self.n -= k
        self.buf &= (1 << self.n) - 1

    def read(self, k):
        if k == 0:
            return 0
        value = self.peek(k)
        self.skip(k)
        return value

    def align_marker(self):
        """丢掉未用尽的比特，跳到下一个标记（restart 用）。"""
        self.n = 0
        self.buf = 0
        d = self.d
        i = self.i
        size = len(d)
        while i + 1 < size:
            if d[i] == 0xFF and d[i + 1] not in (0x00, 0xFF):
                self.i = i
                return d[i + 1]
            i += 1
        self.i = size
        self.eof = True
        return None


class _Huff:
    """JPEG Huffman 表：8 位快表 + 规范式慢路径。"""

    __slots__ = ("fast", "maxcode", "mincode", "valptr", "values")

    def __init__(self, counts, values):
        self.values = values
        self.fast = [None] * 256
        self.maxcode = [-1] * 17
        self.mincode = [0] * 17
        self.valptr = [0] * 17
        code = 0
        k = 0
        for length in range(1, 17):
            cnt = counts[length - 1]
            self.valptr[length] = k
            self.mincode[length] = code
            if cnt:
                for _ in range(cnt):
                    if length <= 8 and code < 256:
                        shift = 8 - length
                        base = code << shift
                        entry = (values[k], length)
                        for fill in range(1 << shift):
                            self.fast[base + fill] = entry
                    k += 1
                    code += 1
                self.maxcode[length] = code - 1
            else:
                self.maxcode[length] = -1
            code <<= 1

    def decode(self, bits):
        entry = self.fast[bits.peek(8)]
        if entry is not None:
            bits.skip(entry[1])
            return entry[0]
        code = bits.read(1)
        length = 1
        while length <= 16 and code > self.maxcode[length]:
            code = (code << 1) | bits.read(1)
            length += 1
        if length > 16:
            raise DecodeError("Huffman 码字非法（熵段损坏）")
        return self.values[self.valptr[length] + code - self.mincode[length]]


def _jpeg_luma(data, dc_only=False):
    pos = 2
    qtables = {}
    htables = {}
    frame = None
    restart_interval = 0
    size = len(data)
    while pos < size:
        if data[pos] != 0xFF:
            pos += 1
            continue
        marker = data[pos + 1] if pos + 1 < size else 0xD9
        if marker in (0xD8, 0x01) or 0xD0 <= marker <= 0xD7:
            pos += 2
            continue
        if marker == 0xD9:
            break
        if pos + 4 > size:
            break
        seglen = struct.unpack(">H", data[pos + 2:pos + 4])[0]
        body = data[pos + 4:pos + 2 + seglen]
        if marker == 0xDB:                              # DQT
            i = 0
            while i < len(body):
                pq, tq = body[i] >> 4, body[i] & 0x0F
                i += 1
                if pq == 0:
                    table = list(body[i:i + 64])
                    i += 64
                else:
                    table = list(struct.unpack(">64H", body[i:i + 128]))
                    i += 128
                # DQT 里本来就是 zigzag 序；_decode_block 的 k 也是 zigzag 下标
                qtables[tq] = table
        elif marker == 0xC4:                            # DHT
            i = 0
            while i + 17 <= len(body):
                tc, th = body[i] >> 4, body[i] & 0x0F
                counts = list(body[i + 1:i + 17])
                total = sum(counts)
                values = list(body[i + 17:i + 17 + total])
                i += 17 + total
                htables[(tc, th)] = _Huff(counts, values)
        elif marker in (0xC0, 0xC1):                    # baseline / extended
            if len(body) < 6:
                raise DecodeError("JPEG SOF 段过短")
            precision, height, width, ncomp = struct.unpack(">BHHB", body[:6])
            if precision != 8:
                raise DecodeError("JPEG 精度 %d 不支持（只支持 8bit）" % precision)
            comps = []
            for c in range(ncomp):
                cid, sampling, tq = struct.unpack(">BBB", body[6 + c * 3:9 + c * 3])
                comps.append({"id": cid, "h": sampling >> 4, "v": sampling & 0x0F,
                              "tq": tq})
            frame = {"width": width, "height": height, "comps": comps}
        elif marker in (0xC2, 0xC3, 0xC5, 0xC6, 0xC7, 0xC9, 0xCA, 0xCB,
                        0xCD, 0xCE, 0xCF):
            raise DecodeError("JPEG 编码方式 0x%02X 不支持（只支持 baseline）" % marker)
        elif marker == 0xDD:                            # DRI
            restart_interval = struct.unpack(">H", body[:2])[0]
        elif marker == 0xDA:                            # SOS → 熵段
            if frame is None:
                raise DecodeError("JPEG 缺 SOF")
            return _jpeg_scan(data, pos + 2 + seglen, frame, qtables, htables,
                             restart_interval, body, dc_only=dc_only)
        pos += 2 + seglen
    raise DecodeError("JPEG 未找到扫描段（SOS）")


def _jpeg_scan(data, start, frame, qtables, htables, restart_interval, sos,
               dc_only=False):
    width = frame["width"]
    height = frame["height"]
    if width * height > MAX_PIXELS:
        raise DecodeError("图像过大：%dx%d" % (width, height))
    ns = sos[0] if sos else len(frame["comps"])
    by_id = {c["id"]: c for c in frame["comps"]}
    comps = []
    for c in range(ns):
        cs, tdta = sos[1 + c * 2], sos[2 + c * 2]
        if cs not in by_id:
            raise DecodeError("JPEG 扫描段引用了未定义的分量 id=%d" % cs)
        comp = dict(by_id[cs])
        comp["dc"] = htables.get((0, tdta >> 4))
        comp["ac"] = htables.get((1, tdta & 0x0F))
        comp["q"] = qtables.get(comp["tq"])
        if comp["dc"] is None or comp["ac"] is None or comp["q"] is None:
            raise DecodeError("JPEG 扫描段引用了未定义的 Huffman/量化表")
        comp["pred"] = 0
        comp["plane"] = None
        comps.append(comp)
    if not comps:
        raise DecodeError("JPEG 扫描段无分量")

    hmax = max(c["h"] for c in comps)
    vmax = max(c["v"] for c in comps)
    mcu_w = 8 * hmax
    mcu_h = 8 * vmax
    mcus_x = (width + mcu_w - 1) // mcu_w
    mcus_y = (height + mcu_h - 1) // mcu_h

    for comp in comps:
        if dc_only:                     # DC 档：一个 DCT 块 → 一个像素
            plane_w = mcus_x * comp["h"]
            plane_h = mcus_y * comp["v"]
        else:
            plane_w = mcus_x * comp["h"] * 8
            plane_h = mcus_y * comp["v"] * 8
        comp["plane_w"] = plane_w
        comp["plane_h"] = plane_h
        comp["plane"] = bytearray(plane_w * plane_h)

    bits = _Bits(data, start)
    block = [0] * 64
    out = [0.0] * 64
    mcu_index = 0
    for my in range(mcus_y):
        for mx in range(mcus_x):
            if restart_interval and mcu_index and mcu_index % restart_interval == 0:
                marker = bits.align_marker()
                if marker is None:
                    raise DecodeError("JPEG 熵段提前结束（缺 RST）")
                bits.i += 2
                bits.eof = False
                for comp in comps:
                    comp["pred"] = 0
            mcu_index += 1
            for comp in comps:
                for by in range(comp["v"]):
                    for bx in range(comp["h"]):
                        _decode_block(bits, comp, block)
                        if dc_only:
                            _store_dc(comp, block, mx * comp["h"] + bx,
                                      my * comp["v"] + by)
                        else:
                            _idct(block, out)
                            _store(comp, out, mx * comp["h"] + bx,
                                   my * comp["v"] + by)

    if dc_only:
        return _compose_dc_luma(comps)
    luma = _compose_luma(comps, width, height)
    return width, height, luma


def _decode_block(bits, comp, block):
    for i in range(64):
        block[i] = 0
    dc = comp["dc"].decode(bits)
    diff = _receive_extend(bits, dc)
    comp["pred"] += diff
    block[0] = comp["pred"] * comp["q"][0]
    k = 1
    ac = comp["ac"]
    while k < 64:
        rs = ac.decode(bits)
        run = rs >> 4
        s = rs & 0x0F
        if s == 0:
            if run == 15:
                k += 16
                continue
            break
        k += run
        if k > 63:
            break
        value = _receive_extend(bits, s) * comp["q"][k]
        block[_ZIGZAG[k]] = value
        k += 1


def _receive_extend(bits, s):
    if s == 0:
        return 0
    value = bits.read(s)
    if value < (1 << (s - 1)):
        value -= (1 << s) - 1
    return value


def _idct(block, out):
    """8x8 反 DCT（可分离 + 行稀疏跳过；输出 float 到 out[64]）。"""
    rows = []
    for v in range(8):
        base = v * 8
        row = block[base:base + 8]
        if not any(row):
            rows.append(None)
            continue
        acc = [0.0] * 8
        for u in range(8):
            coef = row[u]
            if not coef:
                continue
            basis = _COS[u]
            for x in range(8):
                acc[x] += coef * basis[x]
        rows.append(acc)
    # 列方向：out[y*8+x] = 0.25 * Σ_v rows[v][x] · C(v)cos((2y+1)vπ/16)
    for y in range(8):
        cosy = [_COS[v][y] for v in range(8)]
        base = y * 8
        for x in range(8):
            total = 0.0
            for v in range(8):
                row = rows[v]
                if row is not None:
                    total += row[x] * cosy[v]
            out[base + x] = total * 0.25


def _store(comp, out, bx, by):
    plane = comp["plane"]
    pw = comp["plane_w"]
    x0 = bx * 8
    y0 = by * 8
    for y in range(8):
        row = (y0 + y) * pw + x0
        src = y * 8
        for x in range(8):
            value = out[src + x] + 128.0
            if value <= 0.0:
                value = 0
            elif value >= 255.0:
                value = 255
            plane[row + x] = int(value + 0.5)


def _store_dc(comp, block, bx, by):
    """DC 档：DC 系数 /8 + 128 就是一个 8×8 块的平均亮度（JPEG 约定）。"""
    value = block[0] / 8.0 + 128.0
    if value <= 0.0:
        value = 0
    elif value >= 255.0:
        value = 255
    comp["plane"][by * comp["plane_w"] + bx] = int(value + 0.5)


def _compose_dc_luma(comps):
    """DC 平面合成 Rec.709 luma（Cb/Cr 用各自块栅格最近邻放大）。"""
    y_comp = comps[0]
    cb = comps[1] if len(comps) > 1 else None
    cr = comps[2] if len(comps) > 2 else None
    width = y_comp["plane_w"]
    height = y_comp["plane_h"]
    out = bytearray(width * height)
    yplane = y_comp["plane"]
    for y in range(height):
        for x in range(width):
            value = yplane[y * width + x]
            if cb is not None and cr is not None:
                cy = min(cb["plane_h"] - 1, y * cb["plane_h"] // height)
                cx = min(cb["plane_w"] - 1, x * cb["plane_w"] // width)
                cby = min(cr["plane_h"] - 1, y * cr["plane_h"] // height)
                cbx = min(cr["plane_w"] - 1, x * cr["plane_w"] // width)
                value = int(round(value
                                  - 0.21269 * (cr["plane"][cby * cr["plane_w"] + cbx] - 128)
                                  - 0.11819 * (cb["plane"][cy * cb["plane_w"] + cx] - 128)))
                if value < 0:
                    value = 0
                elif value > 255:
                    value = 255
            out[y * width + x] = value
    return width, height, out


def _compose_luma(comps, width, height):
    """按分量角色合成 Rec.709 luma（Y 1:1，Cb/Cr 原分辨率最近邻放大）。"""
    luma = bytearray(width * height)
    y_comp = comps[0]
    cb = comps[1] if len(comps) > 1 else None
    cr = comps[2] if len(comps) > 2 else None
    yplane = y_comp["plane"]
    ypw = y_comp["plane_w"]
    cb_p = cb["plane"] if cb else None
    cr_p = cr["plane"] if cr else None
    cb_w = cb["plane_w"] if cb else 0
    cr_w = cr["plane_w"] if cr else 0
    cb_h = cb["plane_h"] if cb else 0
    cr_h = cr["plane_h"] if cr else 0
    yh = y_comp["plane_h"]
    yw = y_comp["plane_w"]
    for y in range(height):
        yrow = y * ypw
        out = y * width
        cy = min(cb_h - 1, y * cb_h // yh) if cb else 0
        cy2 = min(cr_h - 1, y * cr_h // yh) if cr else 0
        cb_row = cy * cb_w if cb else 0
        cr_row = cy2 * cr_w if cr else 0
        for x in range(width):
            value = yplane[yrow + x]
            if cb_p is not None and cr_p is not None:
                cx = min(cb_w - 1, x * cb_w // yw)
                cx2 = min(cr_w - 1, x * cr_w // yw)
                value = int(round(value
                                  - 0.21269 * (cr_p[cr_row + cx2] - 128)
                                  - 0.11819 * (cb_p[cb_row + cx] - 128)))
                if value < 0:
                    value = 0
                elif value > 255:
                    value = 255
            luma[out + x] = value
    return luma
