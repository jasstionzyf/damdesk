#!/usr/bin/env python3
"""DM-B1（t_7a23bc3b）手验用：造一个**映射外**能力的 `.dampack`。

口径：复制仓库 `packs/plugins/upscale`（能力集在默认宿主能力集内，容器形态也能装），只把
manifest 的 `id` 换成 `com.soujpg.pack.upscale_demo`（仓库里没有这个 id，界面也没有任何
id → 名字映射表）—— 这正是 KI-36 的现象场景：装映射外的能力时界面显示裸 id。

名字仍来自包自己的 manifest（`short_name` = 图片放大 / `summary` = 一句话描述），
所以「装完界面显示中文名」能且只能由接口下发解释。

用法：python3 scripts/dm_b1_demo_pack.py --out /tmp/demo.dampack
"""
from __future__ import annotations

import argparse
import json
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "packs" / "plugins" / "upscale"
DEMO_ID = "com.soujpg.pack.upscale_demo"
SKIP_DIRS = {"tests", "__pycache__"}


def build(dest: Path) -> dict:
    manifest = json.loads((SRC / "manifest.json").read_text(encoding="utf-8"))
    manifest["id"] = DEMO_ID
    raw = json.dumps(manifest, ensure_ascii=False, indent=2).encode("utf-8")
    files: list[tuple[str, bytes]] = [("manifest.json", raw)]
    for p in sorted(SRC.rglob("*")):
        if not p.is_file():
            continue
        rel = p.relative_to(SRC).as_posix()
        if rel == "manifest.json" or any(part in SKIP_DIRS for part in rel.split("/")):
            continue
        files.append((rel, p.read_bytes()))
    dest.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(dest, "w", zipfile.ZIP_DEFLATED) as z:
        for rel, blob in files:
            z.writestr(rel, blob)
    return {"id": DEMO_ID, "version": manifest["version"], "name": manifest["name"], "dest": str(dest)}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True, help="输出 .dampack 路径")
    args = ap.parse_args()
    info = build(Path(args.out))
    print(json.dumps(info, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
