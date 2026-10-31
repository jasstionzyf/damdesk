#!/usr/bin/env python3
"""几何组语料验收（真网关 + 真像素）：resize 10 张 / smart_crop 10 张（N6a）。

与 `test_geometry.py` 的分工：
    单测   —— 纯几何对 labels 真值（不碰像素，毫秒级，`test_gate.sh` 里跑）；
    本脚本 —— **真网关 + 真编解码 + 真产物**：调 `geometry.*_action()` 走
              `/io/read` / `/detect/bbox` / `/op/crop` / `/op/letterbox` / `/op/upscale`，
              把产物写到 work-dir，再逐图核对：

    resize  ① 产物尺寸 == labels.expect.out_size（逐像素）；
            ② 只缩不放：两条边都 ≤ 源图；
            ③ 宿主实际走的是重采样档（`op=upscale.resample`，不误触超分模型）；
            ④ 反向补齐：小图 + 默认不放大 → `skipped(no_upscale)`，一次网关调用都不许发。
    crop    ① 产物比例 == labels.expect.aspect（容差 1px）；
            ② **labels 的主体真值 bbox 完整落在产物内**（贴逐图偏移量，含补边偏移）；
            ③ 裁不下的图必须走 `crop+pad`：产物边框是纯白（`/detect/white_stats` 实测）；
            ④ 产物只落一张（crop+pad 的中间文件必须删掉）。

前置：一个真网关。两种用法——
    A) 本脚本自己拉起 (需要 cargo / 容器内)：
         python3 packs/plugins/imaging/tests/corpus_geometry_e2e.py --spawn-gateway
    B) 外部已起好（`cargo run -p pack-host --example gateway_serve -- --work-dir ...`）：
         python3 .../corpus_geometry_e2e.py --gateway-url http://127.0.0.1:PORT --gateway-token T

环境变量兜底：`DAMPACK_GATEWAY_URL` / `DAMPACK_GATEWAY_TOKEN`；语料路径 `DAM_CORPUS`。
退出码：0 全绿 / 1 有 FAIL（打印逐图表格）。
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
PACK = os.path.dirname(HERE)
REPO = os.path.abspath(os.path.join(PACK, "..", "..", ".."))
PLUGIN = os.path.join(PACK, "plugin")
for p in (PACK, PLUGIN):
    if p not in sys.path:
        sys.path.insert(0, p)

import geometry                                    # noqa: E402
from gw import Gateway, GatewayError               # noqa: E402

DEFAULT_CORPUS = os.environ.get("DAM_CORPUS", "/data/damdesk-test-corpus/v1")
RESIZE_LONG_EDGE = 1600        # 语料 spec：长边 ≤1600 等比
ASPECT_TOL_PX = 1.5            # 比例容差：1.5px（宿主四舍五入带来的抖动）


# ---------------------------------------------------------------- 网关拉起
class SpawnedGateway:
    """`cargo run -p pack-host --example gateway_serve`（容器内用；模型根 DAM_MODELS_ROOT）。"""

    def __init__(self, work_dir):
        self.work_dir = work_dir
        self.proc = None
        self.url = None
        self.token = None

    def __enter__(self):
        cmd = ["cargo", "run", "-q", "-p", "pack-host", "--example", "gateway_serve", "--",
               "--work-dir", self.work_dir,
               "--capabilities", "io.read,detect.u2net,detect.white,op.geometric"]
        env = dict(os.environ)
        env.setdefault("DAM_MODELS_ROOT", "/")
        self.proc = subprocess.Popen(cmd, cwd=REPO, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, stdin=subprocess.PIPE,
                                     text=True, bufsize=1, env=env)
        line = self.proc.stdout.readline()
        if not line.strip():
            err = self.proc.stderr.read() if self.proc.stderr else ""
            raise RuntimeError("gateway_serve 没输出 URL（stderr 尾部：%s）" % err[-2000:])
        info = json.loads(line)
        self.url, self.token = info["url"], info["token"]
        # 等端口可连（cargo 已退出 exec 阶段，通常立刻可连）
        for _ in range(50):
            try:
                urllib.request.urlopen(self.url + "/io/read", data=b"{}", timeout=1)
            except urllib.error.HTTPError:
                break                                  # 401/400 都说明在听
            except Exception:                          # noqa: BLE001
                time.sleep(0.1)
        return self

    def __exit__(self, *exc):
        if self.proc:
            try:
                self.proc.stdin.write("quit\n")
                self.proc.stdin.flush()
            except Exception:                          # noqa: BLE001
                pass
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        return False


# ---------------------------------------------------------------- 判定工具
def _round_half_up(x):
    import math
    return int(math.floor(float(x) + 0.5))


def aspect_ok(w, h, target):
    """比例是否在 1.5px 容差内（避免 2092/2090 这类四舍五入被判错）。"""
    return abs(w / float(h) - target) <= ASPECT_TOL_PX / float(min(w, h)) + 1e-9


def mapped_box(box, fields):
    """把源图坐标框映射到产物坐标：crop 偏移 + （crop+pad 的）居中补边偏移。"""
    x1, y1, x2, y2 = box
    bx1, by1 = fields["box"][0], fields["box"][1]
    ox, oy = (fields.get("padOffset") or [0, 0])
    return [x1 - bx1 + ox, y1 - by1 + oy, x2 - bx1 + ox, y2 - by1 + oy]


def inside(mapped, out_size):
    w, h = out_size
    return (mapped[0] >= 0 and mapped[1] >= 0 and mapped[2] <= w and mapped[3] <= h)


def iou(a, b):
    ax1, ay1, ax2, ay2 = a
    bx1, by1, bx2, by2 = b
    ix = max(0, min(ax2, bx2) - max(ax1, bx1))
    iy = max(0, min(ay2, by2) - max(ay1, by1))
    inter = ix * iy
    if inter <= 0:
        return 0.0
    area = (ax2 - ax1) * (ay2 - ay1) + (bx2 - bx1) * (by2 - by1) - inter
    return inter / float(area) if area else 0.0


def _load(path):
    with open(path, encoding="utf-8") as fh:
        return json.load(fh)


# ---------------------------------------------------------------- resize
def run_resize(gw, corpus, work_dir):
    labels = _load(os.path.join(corpus, "labels", "resize.json"))
    rows, failures = [], 0
    print("\n== resize（长边 %d 等比，不放大）==" % RESIZE_LONG_EDGE)
    print("%-28s %-14s %-14s %-10s %-6s %s"
          % ("file", "in", "out(实际)", "expected", "op", "判定"))
    for it in labels["items"]:
        src = os.path.join(corpus, it["file"])
        exp = it["expect"]
        row = geometry.resize_action({"id": it["file"], "path": src},
                                     {"resizeMode": "long_edge",
                                      "resizeLongEdge": RESIZE_LONG_EDGE}, gw, work_dir)
        ok, why = False, ""
        if row["result"] != "ok":
            why = "%s(%s)" % (row["result"], row.get("error") or row.get("skipReason"))
        else:
            got = row["fields"]["out"]
            if got != exp["out_size"]:
                why = "尺寸 %s != 期望 %s" % (got, exp["out_size"])
            elif got[0] > exp["in_size"][0] or got[1] > exp["in_size"][1]:
                why = "放大了（%s > %s）" % (got, exp["in_size"])
            elif row["evidence"].get("opDetail") != "upscale.resample":
                why = "没走重采样档（op=%s）" % row["evidence"].get("opDetail")
            elif not os.path.isfile(row["fields"]["outPath"]):
                why = "产物文件不存在"
            else:
                ok = True
        failures += 0 if ok else 1
        print("%-28s %-14s %-14s %-10s %-6s %s"
              % (it["file"], "x".join(map(str, exp["in_size"])),
                 "x".join(map(str, row["fields"].get("out", []))),
                 "x".join(map(str, exp["out_size"])),
                 str(row["fields"].get("op") or "-"),
                 "PASS" if ok else "FAIL " + why))
        rows.append({"file": it["file"], "expected": exp["out_size"],
                     "actual": row["fields"].get("out"), "ok": ok, "why": why,
                     "sha256": row["fields"].get("sha256"),
                     "outPath": row["fields"].get("outPath")})

    # 反向补齐：小图 + 默认不放大 → skipped(no_upscale)，且不发起任何像素操作
    small = os.path.join(corpus, labels["items"][0]["file"])
    info = gw.io_read(small)
    plan = geometry.plan_resize((info["width"], info["height"]),
                               {"resizeLongEdge": max(info["width"], info["height"]) * 2})
    skip_row = geometry.resize_action({"id": "small", "path": small},
                                      {"resizeLongEdge": max(info["width"],
                                                             info["height"]) * 2}, gw, work_dir)
    ok = skip_row["result"] == "skipped" and skip_row.get("skipReason") == "no_upscale"
    failures += 0 if ok else 1
    print("%-28s %-14s %-14s %-10s %-6s %s"
          % ("[反向] 小图放大", "x".join(map(str, (info["width"], info["height"]))),
             "-", "-", "-", "PASS 仅缩不放：skipped(no_upscale)" if ok
             else "FAIL %s" % (skip_row,)))
    rows.append({"file": "[反向]no_upscale", "skip": skip_row.get("skipReason"), "ok": ok,
                 "plan": plan})
    return rows, failures


# ---------------------------------------------------------------- crop
def run_crop(gw, corpus, work_dir):
    labels = _load(os.path.join(corpus, "labels", "smart_crop.json"))
    rows, failures = [], 0
    print("\n== smart_crop（比例 + 主体不切）==")
    print("%-26s %-6s %-12s %-14s %-9s %-7s %-7s %s"
          % ("file", "比例", "in", "out(实际)", "mode", "比例✓", "bbox✓", "判定"))
    for it in labels["items"]:
        src = os.path.join(corpus, it["file"])
        exp = it["expect"]
        aspect = geometry.parse_aspect(exp["aspect"])
        row = geometry.crop_action({"id": it["file"], "path": src},
                                   {"cropAspect": exp["aspect"]},   # 用插件默认 cropPadding（5%），验的是发货默认值
                                   gw, work_dir)
        ok, why, ratio_ok, bbox_ok = False, "", False, False
        if row["result"] != "ok":
            why = "%s(%s)" % (row["result"], row.get("error") or row.get("skipReason"))
        else:
            fields = row["fields"]
            ow, oh = fields["out"]
            ratio_ok = aspect_ok(ow, oh, aspect)
            subject = exp.get("subject_bbox")
            mb = mapped_box(subject, fields) if subject else None
            bbox_ok = bool(mb) and inside(mb, (ow, oh)) and fields["subjectCovered"]
            artifacts = [f for f in os.listdir(work_dir) if f.startswith(
                os.path.splitext(os.path.basename(src))[0])]
            if not ratio_ok:
                why = "比例 %.4f != %.4f" % (ow / float(oh), aspect)
            elif not bbox_ok:
                why = "主体超出产物：mapped=%s out=%s" % (mb, (ow, oh))
            elif len(artifacts) != 1:
                why = "产物不止一张：%s" % artifacts
            elif fields["mode"] == "crop+pad":
                stats = gw.call("/detect/white_stats", {"path": fields["outPath"], "band": 4,
                                                       "max_dev": 8})
                if not stats.get("pass"):
                    why = "补边不是纯白（mean_dev=%.2f）" % (stats.get("mean_dev") or -1)
                else:
                    ok = True
            else:
                ok = True
        failures += 0 if ok else 1
        print("%-26s %-6s %-12s %-14s %-9s %-7s %-7s %s"
              % (it["file"], exp["aspect"], "x".join(map(str, exp["in_size"])),
                 "x".join(map(str, row["fields"].get("out", []))),
                 row["fields"].get("mode") or "-",
                 "✓" if ratio_ok else "✗", "✓" if bbox_ok else "✗",
                 "PASS" if ok else "FAIL " + why))
        fields = row["fields"]
        rows.append({
            "file": it["file"], "aspect": exp["aspect"], "in": exp["in_size"],
            "out": fields.get("out"), "mode": fields.get("mode"),
            "feasible": fields.get("feasible"), "box": fields.get("box"),
            "padOffset": fields.get("padOffset"),
            "labelBbox": exp.get("subject_bbox"),
            "detectedBbox": fields.get("subject"),
            "detectedVsLabelIoU": round(iou(fields.get("subject") or [0, 0, 0, 0],
                                            exp.get("subject_bbox") or [0, 0, 0, 0]), 3),
            "mappedLabelBbox": mapped_box(exp["subject_bbox"], fields)
            if exp.get("subject_bbox") else None,
            "ratioOk": ratio_ok, "bboxOk": bbox_ok, "ok": ok, "why": why,
            "sha256": fields.get("sha256"), "outPath": fields.get("outPath"),
        })
    return rows, failures


# ---------------------------------------------------------------- main
def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", default=DEFAULT_CORPUS)
    ap.add_argument("--gateway-url", default=os.environ.get("DAMPACK_GATEWAY_URL"))
    ap.add_argument("--gateway-token", default=os.environ.get("DAMPACK_GATEWAY_TOKEN"))
    ap.add_argument("--spawn-gateway", action="store_true",
                    help="自己拉起 gateway_serve（需要 cargo；容器内用）")
    ap.add_argument("--work-dir", default=None, help="产物目录（默认临时目录）")
    ap.add_argument("--json-out", default=None, help="逐图结果落盘（报告证据用）")
    args = ap.parse_args(argv)

    if not os.path.isfile(os.path.join(args.corpus, "FROZEN")):
        print("语料未冻结/不存在：%s（先跑 build_plugin_corpus.py --verify）" % args.corpus)
        return 2
    # 网关只收绝对路径（协议 §6：path 一律磁盘绝对路径）
    args.corpus = os.path.abspath(args.corpus)

    work_dir = os.path.abspath(args.work_dir or tempfile.mkdtemp(prefix="geom-e2e-"))
    os.makedirs(work_dir, exist_ok=True)
    spawned = None
    if args.spawn_gateway and not args.gateway_url:
        spawned = SpawnedGateway(work_dir)
        spawned.__enter__()
        url, token = spawned.url, spawned.token
    else:
        url, token = args.gateway_url, args.gateway_token
    if not url:
        print("没有网关：给 --gateway-url/--gateway-token 或 --spawn-gateway")
        return 2
    gw = Gateway(base_url=url, token=token)
    print("corpus=%s\nwork_dir=%s\ngateway=%s\nmodels_root=%s"
          % (args.corpus, work_dir, url, os.environ.get("DAM_MODELS_ROOT", "(unset)")))

    try:
        resize_rows, f1 = run_resize(gw, args.corpus, work_dir)
        crop_rows, f2 = run_crop(gw, args.corpus, work_dir)
    finally:
        if spawned:
            spawned.__exit__()

    total = len(resize_rows) + len(crop_rows)
    print("\n---- 汇总：%d 项，PASS %d，FAIL %d ----"
          % (total, total - f1 - f2, f1 + f2))
    if args.json_out:
        with open(args.json_out, "w", encoding="utf-8") as fh:
            json.dump({"corpus": args.corpus, "gateway": url, "workDir": work_dir,
                       "resize": resize_rows, "crop": crop_rows,
                       "failures": f1 + f2}, fh, ensure_ascii=False, indent=1)
        print("逐图结果：%s" % args.json_out)
    return 0 if (f1 + f2) == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
