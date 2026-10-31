#!/usr/bin/env python3
"""编码组语料验收（真网关 + 真编解码 + 真产物）：convert 10 张 / compress 10 张（N6b）。

与 `test_codec.py` 的分工：
    单测   —— 纯策略（计划/质量二分/降采样阶梯）对桩网关，毫秒级，`test_gate.sh` 无关；
    本脚本 —— **真网关 + 真像素 + 真产物**：调 `codec.*_action()` 走 `/io/read` /
              `/op/encode`（+ compress 的 `/op/upscale` 降采样档），产物写 work-dir，
              再用 `codec_probe`（Rust example）解码产物做像素级断言：

    convert  ① 产物格式 == labels.expect.out_format（**魔数**判定，不看扩展名）；
             ② 尺寸不变（convert 不缩放）；
             ③ 透明源 → png 的 alpha 通道**逐像素一致**（alpha 指纹 + 非不透明像素数）；
             ④ EXIF Orientation=6/8 的源默认摆正：尺寸 == 摆正参考、SSIM ≥0.98；
                关掉 `convertAutoOrient` 则保持存储尺寸（方向没生效，可复现）；
             ⑤ avif 输出是真 ISO-BMFF（`ftypavif`），且 avif→jpg 能回来。
    compress ① 产物体积 ≤ labels.expect.target_max_bytes；
             ② **SSIM ≥ labels.expect.ssim_min**（参考 = 源图按产物尺寸 lanczos3 重采样，
                量的只是编码损失，不把降采样当糊图）；
             ③ 质量 ≥ 质量底线（`compressMinQuality`）；降采样长边 ≥ 下限；
             ④ 产物格式 == labels.expect.out_format；
             ⑤ 中间产物（降采样档/探测档）用完即删：work-dir 里只剩最终产物。

前置：一个真网关。两种用法——
    A) 本脚本自己拉起（需要 cargo / 容器内）：
         python3 packs/plugins/imaging/tests/corpus_codec_e2e.py --spawn-gateway
    B) 外部已起好：
         python3 .../corpus_codec_e2e.py --gateway-url http://127.0.0.1:PORT --gateway-token T

环境变量兜底：`DAMPACK_GATEWAY_URL` / `DAMPACK_GATEWAY_TOKEN`；语料路径 `DAM_CORPUS`。
退出码：0 全绿 / 1 有 FAIL（打印逐图表格 + JSON）。
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
FIXTURES = os.path.join(HERE, "fixtures", "codec")
for p in (PACK, PLUGIN):
    if p not in sys.path:
        sys.path.insert(0, p)

import codec                                       # noqa: E402
from gw import Gateway                             # noqa: E402

DEFAULT_CORPUS = os.environ.get("DAM_CORPUS", "/data/damdesk-test-corpus/v1")
TARGET_BYTES = 200 * 1024          # 卡口径：≤200KB
MIN_QUALITY = 60                   # 质量底线（与插件默认一致）
PREFERRED_QUALITY = 80              # 质量目标（达不到就让图小一点，不把质量砸到底线）
MIN_LONG_EDGE = 640

MAGIC_EXPECT = {"jpg": "jpg", "png": "png", "webp": "webp", "avif": "avif"}


# ---------------------------------------------------------------- 网关拉起
class SpawnedGateway:
    """`cargo run -p pack-host --example gateway_serve`（容器内用）。"""

    def __init__(self, work_dir):
        self.work_dir = work_dir
        self.proc = None
        self.url = None
        self.token = None

    def __enter__(self):
        cmd = ["cargo", "run", "-q", "-p", "pack-host", "--example", "gateway_serve", "--",
               "--work-dir", self.work_dir,
               "--capabilities", "io.read,op.geometric"]
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
        for _ in range(60):
            try:
                urllib.request.urlopen(self.url + "/io/read", data=b"{}", timeout=1)
            except urllib.error.HTTPError:
                break
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


# ---------------------------------------------------------------- 像素探针
class Probe:
    """`codec_probe`（Rust example）：解码产物算 SSIM / alpha 指纹 / 魔数尺寸。"""

    def __init__(self):
        self.binary = self._locate()

    def _locate(self):
        target = os.environ.get("CARGO_TARGET_DIR", "")
        if target:
            cand = os.path.join(target, "debug", "examples", "codec_probe")
            if os.path.isfile(cand):
                return cand
        subprocess.run(["cargo", "build", "-q", "-p", "pack-host", "--example", "codec_probe"],
                       cwd=REPO, check=True)
        target = os.environ.get("CARGO_TARGET_DIR") or os.path.join(REPO, "target")
        cand = os.path.join(target, "debug", "examples", "codec_probe")
        if not os.path.isfile(cand):
            raise RuntimeError("codec_probe 没编出来：%s" % cand)
        return cand

    def run(self, *args):
        out = subprocess.run([self.binary] + [str(a) for a in args],
                             capture_output=True, text=True)
        if out.returncode != 0:
            raise RuntimeError("codec_probe %s 失败：%s" % (args, out.stderr.strip()))
        return json.loads(out.stdout)


def _load_json(path):
    with open(path, encoding="utf-8") as fh:
        return json.load(fh)


def _dump_leftovers(work_dir):
    return sorted(f for f in os.listdir(work_dir))


# ---------------------------------------------------------------- convert
def run_convert(gw, probe, corpus, work_dir):
    labels = _load_json(os.path.join(corpus, "labels", "convert.json"))
    rows, failures = [], 0
    print("\n== convert（语料 10 张：目标格式 / alpha / 方向） ==")
    print("%-34s %-8s %-8s %-11s %-9s %s" % ("file", "in", "out", "实际格式", "alpha", "判定"))
    for it in labels["items"]:
        src = os.path.join(corpus, it["file"])
        want = it["expect"]["out_format"]
        item_work = os.path.join(work_dir, "convert")
        os.makedirs(item_work, exist_ok=True)
        row = codec.convert_action({"id": it["file"], "path": src}, {"convertFormat": want},
                                  gw, item_work)
        info_in = probe.run("info", src)
        reason = ""
        got_magic = None
        if row["result"] != "ok":
            reason = "%s(%s)" % (row["result"], (row.get("error") or {}).get("message"))
        else:
            out_path = row["fields"]["outPath"]
            got = probe.run("info", out_path)
            got_magic = got["magic"]
            if got_magic != MAGIC_EXPECT[want]:
                reason = "格式 %s != %s" % (got_magic, want)
            elif [got["width"], got["height"]] != [info_in["width"], info_in["height"]]:
                reason = "尺寸被改了 %sx%s != %sx%s" % (got["width"], got["height"],
                                                        info_in["width"], info_in["height"])
            elif row["evidence"].get("formatMatchesPlan") is not True:
                reason = "结果行与产物格式不一致"
        ok = not reason
        failures += 0 if ok else 1
        print("%-34s %-8s %-8s %-11s %-9s %s" % (
            it["file"], info_in["magic"], got_magic or "-",
            (row["fields"].get("format") if ok else "-"),
            row["fields"].get("alpha", "-") if ok else "-",
            "PASS" if ok else "FAIL " + reason))
        rows.append({"file": it["file"], "expect": want, "actual": got_magic,
                     "alpha": row["fields"].get("alpha"), "outPath": row["fields"].get("outPath"),
                     "inBytes": info_in["bytes"], "outBytes": row["fields"].get("outBytes"),
                     "ok": ok, "why": reason})
    return rows, failures


def run_convert_alpha(gw, probe, work_dir):
    """夹具 alpha 3 张：透明源 → png 必须**逐像素**保留 alpha（贴指纹与统计）。"""
    manifest = _load_json(os.path.join(FIXTURES, "manifest.json"))
    rows, failures = [], 0
    print("\n== convert alpha（夹具 3 张：透明源 → png 保留 alpha） ==")
    print("%-16s %-11s %-9s %-9s %-12s %s" % ("file", "size", "alphaMin", "alpha<255",
                                              "指纹一致", "判定"))
    item_work = os.path.join(work_dir, "alpha")
    os.makedirs(item_work, exist_ok=True)
    for item in manifest["alpha"]:
        src = os.path.join(FIXTURES, item["file"])
        sig_in = probe.run("alpha-sig", src)
        row = codec.convert_action({"id": item["file"], "path": src}, {"convertFormat": "png"},
                                  gw, item_work)
        reason = ""
        sig_out = None
        if row["result"] != "ok":
            reason = "%s(%s)" % (row["result"], (row.get("error") or {}).get("message"))
        else:
            out_path = row["fields"]["outPath"]
            info = probe.run("info", out_path)
            sig_out = probe.run("alpha-sig", out_path)
            if info["magic"] != "png":
                reason = "产物不是 png（%s）" % info["magic"]
            elif not sig_out["alphaBelow255"]:
                reason = "产物没有透明像素（alpha 被丢了）"
            elif sig_out["alphaFingerprint"] != sig_in["alphaFingerprint"]:
                reason = "alpha 指纹不一致（%s vs %s）" % (sig_in["alphaFingerprint"],
                                                           sig_out["alphaFingerprint"])
            elif [info["width"], info["height"]] != [sig_in["width"], sig_in["height"]]:
                reason = "尺寸变了"
        ok = not reason
        failures += 0 if ok else 1
        print("%-16s %-11s %-9s %-9s %-12s %s" % (
            item["file"], "%dx%d" % (sig_in["width"], sig_in["height"]),
            sig_in["alphaMin"], sig_in["alphaBelow255"],
            ("%s" % (sig_out or {}).get("alphaFingerprint", "-")[:10]) if sig_out else "-",
            "PASS" if ok else "FAIL " + reason))
        rows.append({"file": item["file"], "size": [sig_in["width"], sig_in["height"]],
                     "alphaFingerprint": (sig_out or {}).get("alphaFingerprint"),
                     "alphaBelow255": (sig_out or {}).get("alphaBelow255"),
                     "alphaMin": (sig_out or {}).get("alphaMin"),
                     "outPath": row["fields"].get("outPath"), "ok": ok, "why": reason})
    return rows, failures


def run_convert_orient(gw, probe, work_dir):
    """夹具 orient 2 张：EXIF Orientation=6/8 默认摆正；关掉则保持存储尺寸。"""
    manifest = _load_json(os.path.join(FIXTURES, "manifest.json"))
    rows, failures = [], 0
    print("\n== convert EXIF 方向（夹具 2 张：默认摆正 / 可关） ==")
    print("%-14s %-9s %-11s %-11s %-8s %s" % ("file", "orient", "存储尺寸", "产物尺寸",
                                              "SSIM", "判定"))
    item_work = os.path.join(work_dir, "orient")
    os.makedirs(item_work, exist_ok=True)
    for item in manifest["orient"]:
        src = os.path.join(FIXTURES, item["file"])
        ref = os.path.join(FIXTURES, item["ref"])
        row = codec.convert_action({"id": item["file"], "path": src}, {"convertFormat": "png"},
                                  gw, item_work)
        reason, ssim, info = "", None, None
        if row["result"] != "ok":
            reason = "%s(%s)" % (row["result"], (row.get("error") or {}).get("message"))
        else:
            out_path = row["fields"]["outPath"]
            info = probe.run("info", out_path)
            cmp = probe.run("ssim", ref, out_path)
            ssim = cmp["ssim"]
            if [info["width"], info["height"]] != item["uprightSize"]:
                reason = "没摆正：%sx%s != 摆正 %s" % (info["width"], info["height"],
                                                      item["uprightSize"])
            elif row["fields"].get("orientation") != item["orientation"]:
                reason = "结果行方向值 %s != %s" % (row["fields"].get("orientation"),
                                                   item["orientation"])
            elif ssim < 0.98:
                reason = "摆正后与参考不像（SSIM %.4f）" % ssim
        ok = not reason
        failures += 0 if ok else 1
        print("%-14s %-9s %-11s %-11s %-8s %s" % (
            item["file"], item["orientation"],
            "%dx%d" % tuple(item["storedSize"]),
            ("%dx%d" % (info["width"], info["height"])) if info else "-",
            ("%.4f" % ssim) if ssim is not None else "-",
            "PASS" if ok else "FAIL " + reason))
        rows.append({"file": item["file"], "orientation": item["orientation"],
                     "ssim": ssim, "outSize": [info["width"], info["height"]] if info else None,
                     "outPath": row["fields"].get("outPath"), "ok": ok, "why": reason})

        # 反向：关掉自动摆正 → 尺寸回到存储尺寸（证明方向确实是"按 EXIF 摆"的）
        raw = codec.convert_action({"id": item["file"] + "#raw", "path": src},
                                  {"convertFormat": "png", "convertAutoOrient": False},
                                  gw, item_work)
        raw_ok = raw["result"] == "ok" and raw["fields"].get("orientation") == 1
        if raw_ok:
            raw_info = probe.run("info", raw["fields"]["outPath"])
            raw_ok = [raw_info["width"], raw_info["height"]] == item["storedSize"]
        failures += 0 if raw_ok else 1
        print("%-14s %-9s %-11s %-11s %-8s %s" % (
            item["file"] + "#raw", "-", "%dx%d" % tuple(item["storedSize"]),
            "保持存储尺寸" if raw_ok else "-", "-",
            "PASS 关掉即不摆正" if raw_ok else "FAIL 关掉后仍被摆正"))
        rows.append({"file": item["file"] + "#raw", "ok": raw_ok,
                     "why": "" if raw_ok else "auto_orient=false 未生效"})
    return rows, failures


def run_convert_avif(gw, probe, corpus, work_dir):
    """avif 三连：png → avif（真 ISO-BMFF）→ jpg（能回来，且与直转 jpg 不像"两回事"）。"""
    src = os.path.join(corpus, "process/convert/cv_png_04.png")
    if not os.path.isfile(src):
        print("\n== convert avif：SKIP（语料缺 %s）" % src)
        return [], 0
    item_work = os.path.join(work_dir, "avif")
    os.makedirs(item_work, exist_ok=True)
    rows, failures = [], 0
    print("\n== convert avif（png → avif → jpg） ==")

    to_avif = codec.convert_action({"id": "avif#1", "path": src},
                                  {"convertFormat": "avif", "convertQuality": 90}, gw, item_work)
    ok, reason, brand, avif_path = False, "", None, None
    if to_avif["result"] != "ok":
        reason = "%s(%s)" % (to_avif["result"], (to_avif.get("error") or {}).get("message"))
    else:
        avif_path = to_avif["fields"]["outPath"]
        with open(avif_path, "rb") as fh:
            head = fh.read(12)
        brand = head[4:12].decode("latin-1")
        info = gw.io_read(avif_path)        # 尺寸由宿主 vips 解 avif 得到
        if not brand.startswith("ftypavi"):
            reason = "不是 avif 容器（brand=%r）" % brand
        elif [info["width"], info["height"]] != to_avif["fields"]["in"]:
            reason = "avif 尺寸变了 %sx%s" % (info["width"], info["height"])
        else:
            ok = True
    failures += 0 if ok else 1
    print("%-24s brand=%-10s %s" % ("png→avif", brand or "-", "PASS" if ok else "FAIL " + reason))
    rows.append({"step": "png->avif", "brand": brand, "outPath": avif_path, "ok": ok,
                 "why": reason})

    if avif_path:
        to_jpg = codec.convert_action({"id": "avif#2", "path": avif_path},
                                      {"convertFormat": "jpg", "convertQuality": 90}, gw, item_work)
        ok2, reason2, ssim = False, "", None
        if to_jpg["result"] != "ok":
            reason2 = "%s(%s)" % (to_jpg["result"], (to_jpg.get("error") or {}).get("message"))
        else:
            direct = codec.convert_action({"id": "avif#3", "path": src},
                                          {"convertFormat": "jpg", "convertQuality": 90},
                                          gw, item_work)
            cmp = probe.run("ssim", direct["fields"]["outPath"], to_jpg["fields"]["outPath"])
            ssim = cmp["ssim"]
            if ssim < 0.95:
                reason2 = "avif→jpg 与直转 jpg 差异过大（SSIM %.4f）" % ssim
            else:
                ok2 = True
        failures += 0 if ok2 else 1
        print("%-24s ssim=%-10s %s" % ("avif→jpg（对直转 jpg）", "%.4f" % ssim if ssim else "-",
                                       "PASS" if ok2 else "FAIL " + reason2))
        rows.append({"step": "avif->jpg", "ssim": ssim,
                     "outPath": to_jpg["fields"].get("outPath"), "ok": ok2, "why": reason2})
    return rows, failures


# ---------------------------------------------------------------- compress
def run_compress(gw, probe, corpus, work_dir):
    labels = _load_json(os.path.join(corpus, "labels", "compress.json"))
    rows, failures = [], 0
    print("\n== compress（语料 10 张：≤200KB 且 SSIM≥0.95） ==")
    print("%-14s %-10s %-10s %-6s %-7s %-8s %-7s %s" % (
        "file", "in", "out", "q", "降采样", "长边", "SSIM", "判定"))
    for it in labels["items"]:
        src = os.path.join(corpus, it["file"])
        exp = it["expect"]
        target = min(exp["target_max_bytes"], TARGET_BYTES)
        item_work = os.path.join(work_dir, "compress")
        os.makedirs(item_work, exist_ok=True)
        row = codec.compress_action({"id": it["file"], "path": src},
                                   {"compressMode": "target_bytes",
                                    "compressTargetBytes": target,
                                    "compressMinQuality": MIN_QUALITY,
                                    "compressMinLongEdge": MIN_LONG_EDGE,
                                    "compressFormat": exp["out_format"]}, gw, item_work)
        reason, ssim, info = "", None, None
        if row["result"] != "ok":
            reason = "%s(%s)" % (row["result"], (row.get("error") or {}).get("message"))
        else:
            out_path = row["fields"]["outPath"]
            info = probe.run("info", out_path)
            cmp = probe.run("ssim-to", src, out_path)     # 参考按产物尺寸重采样
            ssim = cmp["ssim"]
            long_edge = max(info["width"], info["height"])
            if info["magic"] != MAGIC_EXPECT[exp["out_format"]]:
                reason = "格式 %s != %s" % (info["magic"], exp["out_format"])
            elif row["fields"]["outBytes"] > exp["target_max_bytes"]:
                reason = "体积 %d > %d" % (row["fields"]["outBytes"], exp["target_max_bytes"])
            elif ssim < exp["ssim_min"]:
                reason = "SSIM %.4f < %.2f" % (ssim, exp["ssim_min"])
            elif long_edge < MIN_LONG_EDGE:
                reason = "长边 %d < 下限 %d" % (long_edge, MIN_LONG_EDGE)
            elif row["fields"].get("quality", 0) < MIN_QUALITY:
                reason = "质量 %s < 底线 %d" % (row["fields"].get("quality"), MIN_QUALITY)
            leftovers = [f for f in _dump_leftovers(item_work) if f.startswith(".")]
            if not reason and leftovers:
                reason = "中间产物没删：%s" % leftovers
        ok = not reason
        failures += 0 if ok else 1
        print("%-14s %-10s %-10s %-6s %-7s %-8s %-7s %s" % (
            it["file"], exp["in_bytes"], row["fields"].get("outBytes", "-"),
            row["fields"].get("quality", "-"),
            "是" if row["fields"].get("downscaled") else "否",
            max(info["width"], info["height"]) if info else "-",
            ("%.4f" % ssim) if ssim is not None else "-",
            "PASS" if ok else "FAIL " + reason))
        rows.append({"file": it["file"], "expect": exp,
                     "outBytes": row["fields"].get("outBytes"),
                     "quality": row["fields"].get("quality"),
                     "downscaled": row["fields"].get("downscaled"),
                     "outSize": [info["width"], info["height"]] if info else None,
                     "ssim": ssim, "outPath": row["fields"].get("outPath"),
                     "attempts": row["fields"].get("attempts"),
                     "ok": ok, "why": reason})
    return rows, failures


# ---------------------------------------------------------------- main
def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--spawn-gateway", action="store_true", help="自己拉起真网关（容器内）")
    ap.add_argument("--gateway-url", default=os.environ.get("DAMPACK_GATEWAY_URL", ""))
    ap.add_argument("--gateway-token", default=os.environ.get("DAMPACK_GATEWAY_TOKEN", ""))
    ap.add_argument("--corpus", default=DEFAULT_CORPUS)
    ap.add_argument("--json-out", default="")
    ap.add_argument("--keep-work", action="store_true", help="保留 work-dir（排查用）")
    args = ap.parse_args()
    args.corpus = os.path.abspath(args.corpus)

    if not os.path.isfile(os.path.join(args.corpus, "FROZEN")):
        print("SKIP：语料未就绪（%s）" % args.corpus)
        return 0
    if not os.path.isfile(os.path.join(FIXTURES, "manifest.json")):
        print("FAIL：编码组夹具未生成（跑 python3 %s）"
              % os.path.join(HERE, "make_codec_fixtures.py"))
        return 1

    work_root = tempfile.mkdtemp(prefix="codec-e2e-")
    print("work-dir：%s" % work_root)
    probe = Probe()

    def run(gw):
        total_fail = 0
        report = {}
        for name, fn in (("convert", lambda: run_convert(gw, probe, args.corpus, work_root)),
                         ("convert_alpha", lambda: run_convert_alpha(gw, probe, work_root)),
                         ("convert_orient", lambda: run_convert_orient(gw, probe, work_root)),
                         ("convert_avif", lambda: run_convert_avif(gw, probe, args.corpus, work_root)),
                         ("compress", lambda: run_compress(gw, probe, args.corpus, work_root))):
            rows, fails = fn()
            report[name] = rows
            total_fail += fails
        return total_fail, report

    if args.spawn_gateway:
        with SpawnedGateway(work_root) as spawned:
            gw = Gateway(base_url=spawned.url, token=spawned.token)
            total_fail, report = run(gw)
    elif args.gateway_url:
        gw = Gateway(base_url=args.gateway_url, token=args.gateway_token)
        total_fail, report = run(gw)
    else:
        print("FAIL：要么 --spawn-gateway，要么给 --gateway-url/--gateway-token")
        return 2

    if args.json_out:
        os.makedirs(os.path.dirname(os.path.abspath(args.json_out)), exist_ok=True)
        with open(args.json_out, "w", encoding="utf-8") as fh:
            json.dump(report, fh, ensure_ascii=False, indent=1)
            fh.write("\n")
        print("\n逐图结果：%s" % args.json_out)
    if not args.keep_work:
        shutil.rmtree(work_root, ignore_errors=True)

    print("\n=== 汇总 ===")
    for name, rows in report.items():
        bad = [r for r in rows if not r.get("ok")]
        print("%-16s %2d 项  失败 %d" % (name, len(rows), len(bad)))
    print("总失败：%d" % total_fail)
    return 0 if total_fail == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
