#!/usr/bin/env python3
"""K15 真机行业插件链：造 `.dampack` 归档（官方电商包 + 第三方 SDK 样例包）。

- 官方线：`packs/plugins/ecommerce`（id `com.soujpg.pack.ecommerce`，判定引擎只认这个 id：
  `compliance_api::plugin_pack_dir` 硬编码 PLUGIN_ECOMMERCE_ID）→ 1.0.0 / 1.1.0 两版用于装/切/回滚。
- 第三方线：`tools/pack-sdk-python/examples/ecommerce_py`（id `com.soujpg.pack.ecommerce_py`）
  → 只用于「装/列表/卸载」这组管理面断言（判定不入这条线）。

口径与 `scripts/pack_ui_e2e.py::build_archive` 一致：manifest 原文 sha256 = pluginHash；
`plugin/__init__.py` 里的 `PACK_ID`/`PACK_VERSION` 必须同步改，否则 `/judge` 会 400「插件包与 manifest 不一致」。

用法：python3 scripts/k15_build_packs.py --out <dir>
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import shutil
import tempfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OFFICIAL = ROOT / "packs" / "plugins" / "ecommerce"
THIRD = ROOT / "tools" / "pack-sdk-python" / "examples" / "ecommerce_py"
OFFICIAL_ID = "com.soujpg.pack.ecommerce"
THIRD_ID = "com.soujpg.pack.ecommerce_py"
# 归档不打测试目录（真机不需要；体积与噪声都省）
SKIP_DIRS = {"tests", "__pycache__"}


def build_archive(dest: Path, src: Path, pack_id: str, version: str, name: str) -> dict:
    dest.parent.mkdir(parents=True, exist_ok=True)
    manifest = json.loads((src / "manifest.json").read_text(encoding="utf-8"))
    manifest["id"] = pack_id
    manifest["version"] = version
    manifest["name"] = name
    raw = json.dumps(manifest, ensure_ascii=False, indent=2).encode("utf-8")
    files: list[tuple[str, bytes]] = [("manifest.json", raw)]
    for p in sorted(src.rglob("*")):
        if not p.is_file():
            continue
        rel = p.relative_to(src).as_posix()
        if rel == "manifest.json" or any(part in SKIP_DIRS for part in rel.split("/")):
            continue
        blob = p.read_bytes()
        if rel == "plugin/__init__.py":
            text = blob.decode("utf-8")
            text = re.sub(r'^PACK_ID = ".*"$', f'PACK_ID = "{pack_id}"', text, flags=re.M)
            text = re.sub(r'^PACK_VERSION = ".*"$', f'PACK_VERSION = "{version}"', text, flags=re.M)
            blob = text.encode("utf-8")
        files.append((rel, blob))
    with zipfile.ZipFile(dest, "w", zipfile.ZIP_DEFLATED) as z:
        for rel, blob in files:
            z.writestr(rel, blob)
    return {
        "id": pack_id,
        "version": version,
        "path": str(dest),
        "sha256": hashlib.sha256(dest.read_bytes()).hexdigest(),
        "pluginHash": hashlib.sha256(raw).hexdigest(),
        "files": len(files),
        "bytes": dest.stat().st_size,
    }


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    out = Path(ap.parse_args().out)
    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True)
    result = {
        "official": {
            v: build_archive(out / f"{OFFICIAL_ID}-{v}.dampack", OFFICIAL, OFFICIAL_ID, v,
                             f"电商主图合规（官方包 真机 v{v}）")
            for v in ("1.0.0", "1.1.0")
        },
        "thirdparty": {
            v: build_archive(out / f"{THIRD_ID}-{v}.dampack", THIRD, THIRD_ID, v,
                             f"电商样例包（第三方线 真机 v{v}）")
            for v in ("1.0.0", "1.1.0")
        },
    }
    (out / "archives.json").write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(result, ensure_ascii=False, indent=1))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
