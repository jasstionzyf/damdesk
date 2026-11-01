"""行为化假网关（单测用）：能真的「改变」图像状态，才验得出修复链是否收敛。

conformance 套件的假网关只回固定桩（`fg_ratio` 由文件名决定），所以它验协议面、
验不了算法。这里补一个**有状态**的假网关：图像 = （宽, 高, 主体矩形），
`crop` 按矩形求交（主体被裁掉面积就真的减小）、`letterbox` 补白不改面积、
`upscale` 等比放大，`u2net` 每次都按当前状态算 `fg_ratio`。
于是「居中主体一轮收敛」「贴边主体要第二轮」「主体已占满 → 几何无解」
这三类真实行为都能在单测里跑出来，而不需要真图片和 GPU。
"""
from __future__ import annotations

import hashlib
import json
import os
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def png_size(path):
    """读 PNG IHDR 拿宽高（不引入图像库）：签名 8 字节 + 长度 4 + 'IHDR' + w/h。"""
    try:
        with open(path, "rb") as fh:
            head = fh.read(24)
    except OSError:
        return None
    if len(head) < 24 or head[:8] != b"\x89PNG\r\n\x1a\n" or head[12:16] != b"IHDR":
        return None
    return (int.from_bytes(head[16:20], "big"), int.from_bytes(head[20:24], "big"))


def mask_has_paint(path):
    """掩码里有没有白像素 —— 真 lama 网关对全空掩码直接 500「无待补洞区域」。

    假网关照同一口径卡：插件把**别的画布上量的框**画到当前画布（坐标整块越界）
    写出来的就是全黑掩码，真网关会拒（K07-FIX-G-B f002 实测）。
    """
    try:
        with open(path, "rb") as fh:
            blob = fh.read()
    except OSError:
        return False
    if not blob.startswith(b"\x89PNG\r\n\x1a\n"):
        return False
    pos, idat = 8, b""
    while pos + 8 <= len(blob):
        length = int.from_bytes(blob[pos:pos + 4], "big")
        tag = blob[pos + 4:pos + 8]
        payload = blob[pos + 8:pos + 8 + length]
        pos += 12 + length
        if tag == b"IDAT":
            idat += payload
        elif tag == b"IEND":
            break
    if not idat:
        return False
    import zlib
    try:
        raw = zlib.decompress(idat)
    except zlib.error:
        return False
    return any(byte == 0xFF for byte in raw)


class Image:
    """画布 + 主体矩形（像素坐标，半开区间语义按面积算）。"""

    def __init__(self, width, height, subject=None):
        self.width = int(width)
        self.height = int(height)
        if subject is None:
            side = int(round((0.30 * self.width * self.height) ** 0.5))
            side = max(1, min(side, min(self.width, self.height)))
            x0 = (self.width - side) // 2
            y0 = (self.height - side) // 2
            subject = (x0, y0, x0 + side, y0 + side)
        self.subject = tuple(int(v) for v in subject)

    # --- 面积 / 占比 ---------------------------------------------------
    def area(self):
        x0, y0, x1, y1 = self.subject
        return max(0, x1 - x0) * max(0, y1 - y0)

    def fg_ratio(self):
        return self.area() / float(self.width * self.height)

    # --- 变换（返回新 Image，不改自身） ---------------------------------
    def _spawn(self, width, height, subject):
        """生成同类型的新实例（子类覆写了 fg_ratio 时，变换后仍保留该语义）。"""
        return type(self)(int(width), int(height), subject)

    def cropped(self, box):
        x1, y1, x2, y2 = [int(round(float(v))) for v in box]
        x1, x2 = sorted((x1, x2))
        y1, y2 = sorted((y1, y2))
        clip = self._spawn(max(1, x2 - x1), max(1, y2 - y1), (0, 0, 0, 0))
        sx0, sy0, sx1, sy1 = self.subject
        nx0 = max(sx0, x1) - x1
        ny0 = max(sy0, y1) - y1
        nx1 = max(0, min(sx1, x2) - x1)
        ny1 = max(0, min(sy1, y2) - y1)
        clip.subject = (max(0, nx0), max(0, ny0), max(nx0, nx1), max(ny0, ny1))
        return clip

    def letterboxed(self, target_ratio=1.0):
        side = max(self.width, self.height)
        out = self._spawn(int(round(side * max(1.0, float(target_ratio)))), side,
                          (0, 0, 0, 0))
        dx = (out.width - self.width) // 2
        dy = (out.height - self.height) // 2
        sx0, sy0, sx1, sy1 = self.subject
        out.subject = (sx0 + dx, sy0 + dy, sx1 + dx, sy1 + dy)
        return out

    def upscaled(self, scale):
        scale = float(scale)
        out = self._spawn(max(1, int(round(self.width * scale))),
                          max(1, int(round(self.height * scale))),
                          (0, 0, 0, 0))
        sx0, sy0, sx1, sy1 = self.subject
        out.subject = tuple(int(round(v * scale)) for v in (sx0, sy0, sx1, sy1))
        return out


class FakeImageGateway:
    """12 个端点的有状态桩（走真 HTTP，走真 Bearer 令牌）。"""

    def __init__(self, root, image, *, overlay_lines=(), domain="pass",
                 white_bg="pass", overlay_vl=None, ocr_available=True,
                 bbox_available=True, white_stats_available=True,
                 white_dev=None, supports_measure=True, inpaint_removes_text=True,
                 report_dims=None, dirty_after_geometry=False,
                 domain_samples=None, domain_others=None, domain_cut=False,
                 domain_area=None, domain_subject="商品"):
        self.root = os.path.abspath(root)
        os.makedirs(self.root, exist_ok=True)
        self.token = "behav-" + hashlib.sha256(self.root.encode()).hexdigest()[:12]
        self.images = {}
        self.by_sha = {}          # 内容 sha → 图像状态（插件用 shutil 复制文件时靠它认领）
        self.calls = []
        self.requests = []        # (端点, 请求体)：断言「问了什么/有没有重复问」用
        # --- 检测缓存 + 推理账本（协议 1.1 现状，K07-FIX-D 实测） ----------------
        # 宿主侧口径（`crates/pack-host/src/gateway.rs`）：
        #   · `/detect/u2net`、`/detect/ocr` 的**应答**按 `端点 | 内容 sha | 参数` 缓存
        #     （`cached_detect`）——同一张图问两次 = 只算一遍、第二次直接回缓存；
        #   · `/detect/bbox` 的应答不缓存，但它与 u2net 共用同一份**分割推理**
        #     （`segment_cached`，键含文件长度/mtime）——同一张图连调两个端点也只推理一次。
        # 桩必须如实模拟这两条，否则「调用次数」会被当成「算力次数」断言，
        # 分不清「插件在重复问」和「宿主命中缓存」（K07-FIX-G）。
        # `calls`/`count()` 仍是 HTTP 调用数；`inference_count()` 才是真算力次数。
        self.detect_cache = {}    # (端点, 内容 sha, 参数) → 应答
        self.seg_inferences = []  # 分割推理：按内容 sha 记账（u2net/bbox 共用）
        self.ocr_inferences = []  # OCR 推理：按 (内容 sha, 参数) 记账
        self.cache_hits = {}      # 端点 → 缓存命中次数
        self.emits = []
        self.overlay_lines = list(overlay_lines)
        self.domain = domain
        # K07-FIX-H 翻转保护：按采样序号给出不同答案（None = 每次都一样）
        self.domain_samples = list(domain_samples) if domain_samples else None
        # K07-FIX-I：R0 的结构化应答素材（others = 陪衬物清单；cut = 模型认定主体被裁切）
        self.domain_others = [dict(item) for item in (domain_others or ())]
        self.domain_cut = bool(domain_cut)
        self.domain_area = domain_area
        self.domain_subject = domain_subject
        self.white_bg = white_bg
        self.overlay_vl = overlay_vl          # None = 按 OCR 结果决定
        self.ocr_available = ocr_available
        # 协议 1.1 端点：缺省可用；置 False 模拟「旧宿主（1.0 网关）」
        self.bbox_available = bbox_available
        self.white_stats_available = white_stats_available
        # K07-FIX-D：`/io/read` 的 `measure:["white"]` 回边缘白度（None = 按 white_bg 推定，
        # 换底算子会真的把它压到接近 0 —— 这样「换底后 R2 复判变 pass」是可测的）。
        self.white_dev = white_dev
        # K07 A4 f002 的故障形态：几何算子把主体裁到**贴着画布边缘**时，边带重新带上主体
        # 像素（真机 white_dev 22.57 > 8）→ 「第 1 轮换底修好的白底」被后面一刀弄坏。
        # 打开后假网关照样本行为判定，用来验「画布变了必须允许重做 replace_bg」。
        self.dirty_after_geometry = bool(dirty_after_geometry)
        # 老网关模拟：不认 `measure`（协议 v1.0 的 io.read 没有这个可选字段）
        self.supports_measure = supports_measure
        # False = inpaint 之后文字还在（模拟「补不干净」→ 复判仍 fail，用来验「不重试」）
        self.inpaint_removes_text = inpaint_removes_text
        # 宿主应答尺寸说谎的钩子（K07 A4 实测：crop 应答 2388×2372、落盘却是上一资产的
        # 816×818）。签名为 `(endpoint, new_image) -> (w, h)`，只改应答、不改状态 ——
        # 插件必须按 `/io/read` 的实际尺寸继续，否则按错短边算放大倍数就再也到不了门槛。
        self.report_dims = report_dims
        self._lock = threading.Lock()
        self._httpd = None
        self.initial = image

    # --- 资产登记 -------------------------------------------------------
    def register(self, path, image):
        path = os.path.abspath(path)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        blob = b"\x89PNG\r\n\x1a\n" + os.path.basename(path).encode()
        with open(path, "wb") as fh:
            fh.write(blob)
        self.images[path] = image
        self.by_sha[hashlib.sha256(blob).hexdigest()] = image
        return path

    def seed_asset(self, name):
        path = os.path.join(self.root, "assets", name)
        return self.register(path, self.initial._spawn(self.initial.width,
                                                       self.initial.height,
                                                       self.initial.subject))

    # --- 生命周期 -------------------------------------------------------
    def start(self):
        gw = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):
                pass

            def _send(self, code, payload):
                body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
                self.send_response(code)
                self.send_header("content-type", "application/json; charset=utf-8")
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                length = int(self.headers.get("content-length") or 0)
                raw = self.rfile.read(length) if length else b"{}"
                try:
                    payload = json.loads(raw.decode("utf-8") or "{}")
                except Exception:
                    payload = {}
                token = (self.headers.get("authorization") or "").replace("Bearer ", "").strip()
                if token != gw.token:
                    return self._send(401, {"error": "unauthorized"})
                with gw._lock:
                    gw.calls.append(self.path)
                    gw.requests.append((self.path, payload))
                try:
                    code, result = gw.dispatch(self.path, payload)
                except KeyError as exc:
                    return self._send(400, {"error": "invalid_params", "detail": str(exc)})
                except FileNotFoundError as exc:
                    return self._send(404, {"error": "asset_unreadable", "detail": str(exc)})
                return self._send(code, result)

        self._httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self._httpd.serve_forever, daemon=True).start()
        return self

    @property
    def base_url(self):
        host, port = self._httpd.server_address[:2]
        return "http://%s:%d" % (host, port)

    def stop(self):
        if self._httpd is not None:
            self._httpd.shutdown()
            self._httpd.server_close()

    def count(self, path):
        return sum(1 for c in self.calls if c == path)

    # --- 推理账本（见 `__init__` 里的宿主口径说明） -------------------------
    def inference_count(self, path):
        """真算力次数：`/detect/u2net` 与 `/detect/bbox` 记同一份分割推理，`/detect/ocr` 单记。"""
        if path in ("/detect/u2net", "/detect/bbox"):
            return len(self.seg_inferences)
        if path == "/detect/ocr":
            return len(self.ocr_inferences)
        raise ValueError("不记推理账的端点：%s" % path)

    def cache_hit_count(self, path=None):
        if path is None:
            return sum(self.cache_hits.values())
        return int(self.cache_hits.get(path, 0))

    def _content_digest(self, path):
        """宿主缓存键的「内容 sha」：同路径换内容必须重算（`detect_cache_key` 同口径）。"""
        try:
            with open(os.path.abspath(str(path)), "rb") as fh:
                return hashlib.sha256(fh.read()).hexdigest()
        except OSError:
            return None

    def _cached_detect(self, endpoint, payload, compute):
        """`cached_detect`：应答按 `端点 | 内容 sha | 参数` 复用（路径不进参数体）。"""
        digest = self._content_digest((payload or {}).get("path"))
        if digest is None:
            return compute()
        params = {k: v for k, v in (payload or {}).items() if k != "path"}
        key = (endpoint, digest, json.dumps(params, sort_keys=True, ensure_ascii=False))
        if key in self.detect_cache:
            self.cache_hits[endpoint] = self.cache_hits.get(endpoint, 0) + 1
            return json.loads(json.dumps(self.detect_cache[key]))   # 缓存命中：不重算
        out = compute()
        self.detect_cache[key] = out
        return out

    def _seg_inference(self, real):
        """`segment_cached`：按内容记账，u2net 与 bbox 共用同一份推理。"""
        digest = self._content_digest(real)
        if digest not in self.seg_inferences:
            self.seg_inferences.append(digest)

    def _ocr_inference(self, real, params):
        digest = self._content_digest(real)
        sig = (digest, json.dumps(params, sort_keys=True, ensure_ascii=False))
        if sig not in self.ocr_inferences:
            self.ocr_inferences.append(sig)

    def state_of(self, path):
        """取某文件的图像状态（插件用 shutil 复制出的文件按内容 sha 认领）。"""
        return self._state(path)[1]

    # --- 端点 -----------------------------------------------------------
    def _state(self, path):
        """按路径取状态；插件用 shutil 复制出来的文件按内容 sha 认领同一份状态。"""
        path = os.path.abspath(str(path))
        if not os.path.isfile(path):
            raise FileNotFoundError(path)
        image = self.images.get(path)
        if image is None:
            with open(path, "rb") as fh:
                image = self.by_sha.get(hashlib.sha256(fh.read()).hexdigest())
            if image is None:
                raise FileNotFoundError(path)
            self.images[path] = image
        return path, image

    def dispatch(self, path, payload):
        if path == "/io/read":
            return self._io_read(payload)
        if path == "/detect/u2net":
            # 应答缓存（宿主 `cached_detect`）：同一张图问两次不会重算
            return self._cached_detect(path, payload, lambda: self._u2net(payload))
        if path == "/detect/bbox":
            if not self.bbox_available:
                return 404, {"error": "no_such_endpoint", "path": path}
            return self._bbox(payload)
        if path == "/detect/white_stats":
            if not self.white_stats_available:
                return 403, {"error": "capability_missing", "capability": "detect.white"}
            return self._white_stats(payload)
        if path == "/detect/ocr":
            return self._cached_detect(path, payload, lambda: self._ocr(payload))
        if path in ("/op/crop", "/op/letterbox", "/op/upscale",
                    "/op/replace_bg", "/op/inpaint"):
            return self._op(path, payload)
        if path == "/vl/judge":
            return self._vl(payload)
        if path == "/report/emit":
            self.emits.append((payload.get("job_id"), payload.get("entry")))
            return 200, {"ok": True}
        return 404, {"error": "no_such_endpoint", "path": path}

    def _io_read(self, payload):
        real, image = self._state(payload.get("path"))
        with open(real, "rb") as fh:
            digest = hashlib.sha256(fh.read()).hexdigest()
        out = {"path": real, "size": os.path.getsize(real),
               "sha256": digest,
               "mime": "image/png",
               "width": image.width, "height": image.height}
        if self.supports_measure and "white" in (payload.get("measure") or []):
            out["white_dev"] = self._white_dev()
            out["white_worst_dev"] = out["white_dev"]
            out["white_band"] = 4
        return 200, out

    def _white_dev(self):
        """边缘白度（K07-FIX-D）：显式值优先，否则按 white_bg 桩推定。"""
        if self.white_dev is not None:
            return float(self.white_dev)
        return 1.0 if self.white_bg == "pass" else 40.0

    def _u2net(self, payload):
        real, image = self._state(payload.get("path"))
        self._seg_inference(real)          # 与 bbox 共用同一份分割推理
        mask = os.path.join(self.root, "masks", os.path.basename(real) + ".mask")
        os.makedirs(os.path.dirname(mask), exist_ok=True)
        open(mask, "wb").close()
        return 200, {"path": real, "fg_ratio": image.fg_ratio(), "mask_path": mask,
                     "width": image.width, "height": image.height}

    def _bbox(self, payload):
        """主体框（协议 1.1）：主体状态就是真值；给 target_ratio 时按面积推窗。"""
        real, image = self._state(payload.get("path"))
        self._seg_inference(real)          # `segment_cached`：与 u2net 共用同一份推理
        sx0, sy0, sx1, sy1 = image.subject
        bbox = [max(0, min(int(sx0), image.width)), max(0, min(int(sy0), image.height)),
                max(0, min(int(sx1), image.width)), max(0, min(int(sy1), image.height))]
        result = {"path": real, "width": image.width, "height": image.height,
                  "fg_ratio": image.fg_ratio(), "engine": "behav-u2net", "grid": 320,
                  "bbox": bbox, "dominant_bbox": list(bbox), "window": None}
        target = payload.get("target_ratio")
        if target is not None:
            target = float(target)
            area = image.area()
            side = int(round((area / target) ** 0.5)) if area > 0 else 0
            side = max(1, min(side, image.width, image.height))
            cx, cy = (bbox[0] + bbox[2]) / 2.0, (bbox[1] + bbox[3]) / 2.0
            wx = int(round(max(0, min(cx - side / 2.0, image.width - side))))
            wy = int(round(max(0, min(cy - side / 2.0, image.height - side))))
            box = [wx, wy, wx + side, wy + side]
            result["window"] = {"box": box,
                                "ratio": round(area / float(side * side), 4),
                                "target_ratio": target}
        return 200, result

    def _white_stats(self, payload):
        """白边统计（协议 1.1）：白底状态 → mean_dev（换底成功即纯白）。"""
        real, image = self._state(payload.get("path"))
        threshold = float(payload.get("max_dev", 8))
        dev = self._white_dev()
        band = int(payload.get("band") or 4)
        return 200, {"path": real, "width": image.width, "height": image.height,
                     "engine": "behav-white-edge", "mean_dev": dev, "worst_dev": dev,
                     "band": band, "edge_pixels": 2 * band * (image.width + image.height),
                     "threshold": threshold, "pass": dev <= threshold}

    def _ocr(self, payload):
        real, image = self._state(payload.get("path"))
        params = {"lang": payload.get("lang")}
        self._ocr_inference(real, params)
        if not self.ocr_available:
            return 200, {"path": real, "lines": []}
        lines = [dict(text=text, box=box) for text, box in self.overlay_lines]
        return 200, {"path": real, "lines": lines}

    def _remap_overlay(self, op, payload, old, new):
        """几何算子改画布后，OCR 框要跟着落到**新画布**上（真宿主：对裁剪/补白后的文件重跑 OCR，
        拿到的就是新坐标）。桩不跟着搬，就会把「旧画布坐标」当成新画布的框，测不出坐标串画布的问题
        （K07-FIX-G-B：插件复用旧框写到新画布 → 全空掩码 → 真网关 500）。"""
        if not self.overlay_lines:
            return
        if op == "/op/crop":
            x1, y1, x2, y2 = [int(round(float(v))) for v in (payload.get("box") or [])]
            x1, x2 = sorted((x1, x2))
            y1, y2 = sorted((y1, y2))
            kept = []
            for text, (bx0, by0, bx1, by1) in self.overlay_lines:
                nb = [max(0, bx0 - x1), max(0, by0 - y1),
                      min(new.width, bx1 - x1), min(new.height, by1 - y1)]
                if nb[2] <= nb[0] or nb[3] <= nb[1]:
                    continue               # 裁到画布外 → 这份文件上根本看不见这些字
                kept.append((text, nb))
            self.overlay_lines = kept
        elif op == "/op/letterbox":
            dx = (new.width - old.width) // 2
            dy = (new.height - old.height) // 2
            self.overlay_lines = [(t, [b[0] + dx, b[1] + dy, b[2] + dx, b[3] + dy])
                                  for t, b in self.overlay_lines]
        elif op == "/op/upscale":
            factor = new.width / float(old.width or 1)
            self.overlay_lines = [(t, [int(round(v * factor)) for v in b])
                                  for t, b in self.overlay_lines]

    def _op(self, path, payload):
        src, image = self._state(payload.get("path"))
        out = os.path.abspath(str(payload.get("out") or ""))
        if not out:
            raise KeyError("out")
        if not (out == self.root or out.startswith(self.root + os.sep)):
            return 400, {"error": "out_of_workroot", "detail": out}
        if path == "/op/inpaint":
            # 真网关口径（`op_inpaint`）：掩码必须与**这张图**同宽高，否则 400。
            # 假网关也照此校验，掩码串了尺寸（K07 A4 f002 实测）才测得出。
            mask = os.path.abspath(str(payload.get("mask") or ""))
            size = png_size(mask)
            if size != (image.width, image.height):
                return 400, {"error": "invalid_params", "op": path,
                             "detail": "掩码尺寸 %s 与图面 %dx%d 不符"
                                       % (size, image.width, image.height),
                             "params": payload}
            # 真 lama 口径：掩码全空 → 500「无待补洞区域（拒绝无意义推理）」。
            # 插件把别的画布上量的框画到当前画布时坐标整块越界 → 写出的就是全黑掩码。
            if not mask_has_paint(mask):
                return 500, {"error": "lama_failed",
                             "detail": "inpaint: 参数错误: 掩码全空：无待补洞区域（拒绝无意义推理）",
                             "op": path, "params": payload}
        if path == "/op/crop":
            new = image.cropped(payload.get("box") or [0, 0, image.width, image.height])
        elif path == "/op/letterbox":
            new = image.letterboxed(payload.get("target_ratio") or 1.0)
        elif path == "/op/upscale":
            scale = float(payload.get("scale") or 1.0)
            # 与真网关同口径的硬门槛（K07 D7 修完后真网关的分趟上限）：
            # 像素 ≤1e8、长边 ≤20000；超了回 400 + 可用上限（假网关也要能验这条）
            tw, th = int(round(image.width * scale)), int(round(image.height * scale))
            if tw * th > 100_000_000 or max(tw, th) > 20_000:
                return 400, {"error": "invalid_params",
                             "detail": "upscale 目标 %dx%d 超上限（像素 ≤1e8、长边 ≤20000）" % (tw, th),
                             "op": path, "params": payload}
            new = image.upscaled(scale)
        else:                                    # replace_bg / inpaint 不改几何
            new = image._spawn(image.width, image.height, image.subject)
            if path == "/op/inpaint":
                if self.inpaint_removes_text:
                    self.overlay_lines = []
            if path == "/op/replace_bg":
                self.white_bg = "pass"           # 换底真的把背景变白了（VL 下次会看见）
                self.white_dev = 0.5              # classic 口径也要跟着变（否则复判看不到修复效果）
        self.register(out, new)
        if path in ("/op/crop", "/op/letterbox", "/op/upscale"):
            self._remap_overlay(path, payload, image, new)
        if self.dirty_after_geometry and path in ("/op/crop", "/op/letterbox", "/op/upscale"):
            # 主体贴到画布边缘 → 边带带上主体像素，白底判据重新 fail（f002 实测形态）
            sx0, sy0, sx1, sy1 = new.subject
            if sx0 <= 0 or sy0 <= 0 or sx1 >= new.width or sy1 >= new.height:
                self.white_bg = "fail"
                self.white_dev = 40.0
        width, height = new.width, new.height
        if self.report_dims is not None:
            lied = self.report_dims(path, new)
            if lied:
                width, height = int(lied[0]), int(lied[1])
        return 200, {"path": out, "width": width, "height": height,
                     "op": path.rsplit("/", 1)[-1]}

    def domain_answer(self, status=None, *, subject_area=None):
        """R0 的结构化应答（K07-FIX-I）：主体在场/面积/cut + others 清单 + verdict。

        `status` 取 `pass` / `fail` / `missing`；`missing` = 画面里没有商品主体。
        与插件判据解耦：桩只给**事实**（面积、清单、verdict），三项否决由插件自己算。
        """
        status = str(status or self.domain or "pass")
        present = status != "missing"
        area = subject_area if subject_area is not None else self.domain_area
        if area is None:
            area = round(self.image_of_domain().fg_ratio(), 6)
        return {"subject": {"present": present, "name": self.domain_subject,
                            "area_ratio": area, "cut": self.domain_cut},
                "others": [dict(item) for item in self.domain_others],
                "verdict": "pass" if status == "pass" else "fail",
                "reason": "stub:%s" % status}

    def image_of_domain(self):
        """当前 R0 判定的那张图（单资产用例就是唯一注册的图）。"""
        return next(iter(self.images.values()))

    def _vl(self, payload):
        prompt = str(payload.get("prompt") or "")
        if "你是电商主图合规审核员" in prompt:
            answer = self.domain
        elif "纯白色" in prompt or "白底" in prompt:
            answer = self.white_bg
        elif "促销文字" in prompt or "水印" in prompt:
            answer = self.overlay_vl or ("fail" if self.overlay_lines else "pass")
        else:
            answer = "pass"
        samples = int(payload.get("samples") or 1)
        domain_rule = "你是电商主图合规审核员" in prompt
        raws = []
        for i in range(max(1, samples)):
            step = answer
            if domain_rule and self.domain_samples:
                step = self.domain_samples[i % len(self.domain_samples)]
            if domain_rule:
                # R0：结构化 JSON（插件按 JSON 解析 + 三项否决）；status 字段留作人读
                body = self.domain_answer(step)
                raw = json.dumps(body, ensure_ascii=False)
                status = "pass" if body["verdict"] == "pass" else "fail"
            else:
                raw = json.dumps({"status": step}, ensure_ascii=False)
                status = step
            raws.append({"path": payload.get("path"), "undetermined": False,
                         "raw": raw,
                         "rules": [{"status": status, "label": "vl", "score": 0.9}]})
        out = dict(raws[0])
        if samples > 1:
            out["raws"] = raws          # 协议 1.2：N 份采样的原始正文
        return 200, out
