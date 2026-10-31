#!/usr/bin/env python3
"""房产包语料矩阵：真图跑六条规则，出「图 × 规则」矩阵（含转人工项）。

跑法（仓根或包目录都行）：
    python3 packs/plugins/realestate/tests/matrix_corpus.py \
        --corpus /data --groups prop-suite,prop-incomplete

两档 VL：
- `--provider live`（默认）：走**云端 deepseek-flash**（用户裁决的默认 VL 通道），
  key 从 profile config 的 `providers.custom.api_key` 读（也可 `--key-file` / 环境
  `DEEPSEEK_API_KEY`），只经请求头传，**不落报告/文件**；
- `--provider stub`：VL 一律回「是」/首个场景词 —— 只能验矩阵管线，不能当判定结论。

产出：报告 markdown（`--out`）+ 机器可读 JSON（`--json`）。F1/F2/F3/F5 是包内
确定性测量（真跑真算），F0/F4 是 VL（live 档就是真模型；报告里会标注通道）。
"""
from __future__ import annotations

import argparse
import base64
import json
import os
import struct
import sys
import tempfile
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
PACK_DIR = os.path.dirname(HERE)
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(PACK_DIR, "plugin"))

import _support          # noqa: E402
import ruleset           # noqa: E402

VL_MODEL = "deepseek-flash"
VL_BASE = "https://api.deepseek.com/v1"
# --provider stub 时用的场景答案（只为让矩阵管线跑通，不构成判定结论）
STUB_SCENES = ["客厅", "卧室", "厨房", "卫生间"]


def jpeg_dims(path):
    """只读 JPEG/PNG 头部拿尺寸（矩阵用；不解码像素）。"""
    with open(path, "rb") as fh:
        data = fh.read(65536)
    if data[:8] == b"\x89PNG\r\n\x1a\n":
        return struct.unpack(">II", data[16:24])
    pos = 2
    while pos + 9 < len(data):
        if data[pos] != 0xFF:
            pos += 1
            continue
        marker = data[pos + 1]
        seglen = struct.unpack(">H", data[pos + 2:pos + 4])[0]
        if marker in (0xC0, 0xC1, 0xC2):
            height, width = struct.unpack(">HH", data[pos + 5:pos + 9])
            return width, height
        pos += 2 + seglen
    return 0, 0


def read_api_key(explicit):
    if explicit:
        return open(explicit, encoding="utf-8").read().strip()
    env = os.environ.get("DEEPSEEK_API_KEY")
    if env:
        return env.strip()
    config = os.path.expanduser("~/.agent/profiles/soujpg/config.yaml")
    if os.path.isfile(config):
        section = None
        for line in open(config, encoding="utf-8"):
            indent = len(line) - len(line.lstrip())
            stripped = line.strip()
            if indent == 2 and stripped.endswith(":"):
                section = stripped[:-1]
            elif section == "custom" and stripped.startswith("api_key:"):
                return stripped.split(":", 1)[1].strip()
    return ""


class LiveVlGateway(_support.StubGateway):
    """网关桩 + 真 VL：/vl/judge 打云端 deepseek-flash，其余端点保持桩。"""

    def __init__(self, *args, api_key="", **kwargs):
        super().__init__(*args, **kwargs)
        self.api_key = api_key
        self.vl_calls = 0

    def _vl(self, payload):
        target = str(payload.get("path") or "")
        if not os.path.isfile(target):
            raise _support._Err(404, "asset_unreadable")
        prompt = str(payload.get("prompt") or "")
        with open(target, "rb") as fh:
            encoded = base64.b64encode(fh.read()).decode("ascii")
        body = {
            "model": VL_MODEL,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": prompt},
                    {"type": "image_url",
                     "image_url": {"url": "data:image/jpeg;base64," + encoded}},
                ],
            }],
            "max_tokens": 4096,
            "temperature": 0,
        }
        request = urllib.request.Request(
            VL_BASE + "/chat/completions",
            data=json.dumps(body, ensure_ascii=False).encode("utf-8"),
            headers={"content-type": "application/json",
                     "authorization": "Bearer " + self.api_key},
            method="POST",
        )
        self.vl_calls += 1
        try:
            with urllib.request.urlopen(request, timeout=120) as resp:
                payload_out = json.loads(resp.read().decode("utf-8"))
        except Exception as exc:                       # 网络/接口故障如实上报
            return 200, {"path": target, "undetermined": True,
                         "reason": "vl_unreachable:%r" % (exc,), "raw": ""}
        content = ""
        try:
            content = (payload_out["choices"][0]["message"].get("content") or "").strip()
        except Exception:
            content = ""
        if not content:
            return 200, {"path": target, "undetermined": True,
                         "reason": "vl_empty_content", "raw": ""}
        return 200, {"path": target, "undetermined": False, "raw": content}


class StubVlGateway(_support.StubGateway):
    """离线档：按目录顺序发场景词，域 gate 一律「是」（只验管线）。"""

    def _vl(self, payload):
        target = str(payload.get("path") or "")
        prompt = str(payload.get("prompt") or "")
        if _support.SCENE_PROMPT_MARK in prompt:
            name = os.path.basename(target)
            index = sum(ord(c) for c in name) % len(STUB_SCENES)
            return 200, {"path": target, "undetermined": False,
                         "raw": STUB_SCENES[index]}
        return 200, {"path": target, "undetermined": False, "raw": "是"}


def run_group(pack_dir, group_dir, api_key, live):
    """跑一个目录（= 一个组）的全部图像，返回 (rows, gateway)。"""
    names = sorted(name for name in os.listdir(group_dir)
                   if name.lower().endswith(ruleset_imgexts()))
    paths = [os.path.join(group_dir, name) for name in names]
    dims = {os.path.abspath(path): jpeg_dims(path) for path in paths}
    workroot = tempfile.mkdtemp(prefix="realestate-matrix-")
    caps = json.load(open(os.path.join(PACK_DIR, "manifest.json"),
                          encoding="utf-8"))["capabilities"]
    gateway_cls = LiveVlGateway if live else StubVlGateway
    if live:
        gateway = gateway_cls(caps, workroot, dims=dims, api_key=api_key).start()
    else:
        gateway = gateway_cls(caps, workroot, dims=dims).start()
    rows = []
    try:
        with _support.Pack(workroot, gateway=gateway) as pack:
            # 组级规则（F3/F4）需要「成员已被判定」的状态才能按 C11 排除非房源图，
            # 因此跑两轮：第二轮即收敛结果（第一轮同时预热 pHash/场景缓存）。
            for pass_no in (1, 2):
              for index, path in enumerate(paths, 1):
                started = time.time()
                reply = pack.call("judge", {"job_id": "matrix", "asset":
                                            {"id": os.path.basename(path),
                                             "path": path},
                                            "options": {"workdir": workroot}},
                                  timeout=300)
                elapsed = time.time() - started
                if pass_no == 1:
                    continue
                if "error" in reply:
                    rows.append({"id": os.path.basename(path), "error": reply["error"],
                                 "seconds": round(elapsed, 1), "verdicts": []})
                    continue
                rows.append({"id": os.path.basename(path),
                             "verdicts": reply["result"]["verdicts"],
                             "summary": reply["result"]["summary"],
                             "seconds": round(elapsed, 1)})
                print("  [%d/%d] %s %.1fs" % (index, len(paths),
                                              os.path.basename(path), elapsed),
                      flush=True)
    finally:
        gateway.stop()
    return rows, gateway


def ruleset_imgexts():
    return (".jpg", ".jpeg", ".png")


def render(rows, group, live, vl_calls):
    rule_ids = ruleset.PIPELINE_ORDER
    lines = []
    lines.append("### %s（%d 张）" % (group, len(rows)))
    lines.append("")
    header = "| 资产 | " + " | ".join(rule_ids) + " | 耗时 |"
    lines.append(header)
    lines.append("|" + "---|" * (len(rule_ids) + 2))
    for row in rows:
        if row.get("error"):
            lines.append("| %s | " % row["id"] + " | ".join(["err"] * len(rule_ids))
                         + " | %ss |" % row["seconds"])
            continue
        cells = []
        by_rule = {v["ruleId"]: v for v in row["verdicts"]}
        for rule_id in rule_ids:
            verdict = by_rule.get(rule_id) or {}
            status = verdict.get("status") or "-"
            score = verdict.get("score")
            mark = {"pass": "pass", "fail": "**FAIL**", "undetermined": "undet",
                    "skipped": "skip"}.get(status, status)
            cells.append("%s%s" % (mark, "" if score is None else " %.3g" % score))
        lines.append("| %s | %s | %ss |" % (row["id"], " | ".join(cells), row["seconds"]))
    lines.append("")
    manual = []
    for row in rows:
        for verdict in row.get("verdicts") or []:
            if verdict.get("status") == "undetermined" or \
                    verdict.get("label") == "manual_review_required":
                manual.append("%s / %s / %s" % (row["id"], verdict["ruleId"],
                                                verdict.get("note")))
    if manual:
        lines.append("转人工项（undetermined）：")
        for item in manual:
            lines.append("- " + item)
    else:
        lines.append("转人工项：无")
    lines.append("")
    return "\n".join(lines)


def main(argv):
    parser = argparse.ArgumentParser()
    parser.add_argument("--corpus", default="/data")
    parser.add_argument("--groups", default="prop-suite,prop-incomplete")
    parser.add_argument("--provider", choices=("live", "stub"), default="live")
    parser.add_argument("--key-file", default="")
    parser.add_argument("--out", default="")
    parser.add_argument("--json", default="")
    args = parser.parse_args(argv)

    live = args.provider == "live"
    api_key = read_api_key(args.key_file) if live else ""
    if live and not api_key:
        print("缺 VL key（--key-file / DEEPSEEK_API_KEY / profile config）", file=sys.stderr)
        return 2

    sections = []
    payload = {"provider": args.provider, "vl_model": VL_MODEL if live else "stub",
               "corpus": args.corpus, "groups": {}}
    for group in [g.strip() for g in args.groups.split(",") if g.strip()]:
        group_dir = os.path.join(args.corpus, group)
        if not os.path.isdir(group_dir):
            print("跳过不存在的组：%s" % group_dir, file=sys.stderr)
            continue
        print("== %s ==" % group, flush=True)
        rows, gateway = run_group(PACK_DIR, group_dir, api_key, live)
        sections.append(render(rows, group, live, getattr(gateway, "vl_calls", 0)))
        payload["groups"][group] = {
            "vl_calls": getattr(gateway, "vl_calls", 0),
            "rows": [{"id": row["id"], "seconds": row["seconds"],
                      "verdicts": row.get("verdicts") or [],
                      "summary": row.get("summary")} for row in rows],
        }

    header = ["## 语料矩阵（%s）" % ("live: %s" % VL_MODEL if live else "stub VL"),
              "",
              "F0/F4 = VL（%s）；F1/F2/F3/F5 = 包内确定性测量（真解码真计算）。" %
              ("云端 deepseek-flash" if live else "桩"),
              ""]
    text = "\n".join(header) + "\n" + "\n".join(sections)
    if args.out:
        os.makedirs(os.path.dirname(os.path.abspath(args.out)), exist_ok=True)
        with open(args.out, "w", encoding="utf-8") as fh:
            fh.write(text)
        print("报告 → %s" % args.out)
    if args.json:
        with open(args.json, "w", encoding="utf-8") as fh:
            json.dump(payload, fh, ensure_ascii=False, indent=2)
        print("JSON → %s" % args.json)
    print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
