#!/usr/bin/env python3
"""造 `.dampack` 归档。两种模式，**别混用**：

## 1. 发布模式（交付给用户/真机的包）—— `--release-version`
```bash
python3 scripts/k15_build_packs.py --release-version 1.1.2 \\
    --out ~/deliver/dampacks-<短sha>/com.soujpg.pack.ecommerce-1.1.2.dampack --record
```
- 只出**官方电商包**（`packs/plugins/ecommerce`，id `com.soujpg.pack.ecommerce`，判定引擎只认这个 id）；
- 出包前过 `scripts/pack_release_guard.py` 硬校验（台账 `packs/pack-content-lock.json`）：
  **内容相对上一发布版本变了却没升 PACK_VERSION → 直接失败**，不落任何产物；
  内容没变则允许同版本重导（幂等）；
- `--record` 才会把本次内容摘要写进台账（发布成功后才加；先出包后入账）。

为什么要这道门（真机卡 t_d84301f5 实锤）：宿主按协议 §7 铁律 7 拒绝「同 id@version 换内容」
（409 `pack_version_conflict`「改内容必须升 version」），而内置包 `pack_builtin_protected` 不可卸载 →
**任何装过旧版本的机器永远升不上去**。宿主的保护是对的，出包流程必须自己挡住这种包。

## 2. 夹具模式（默认，E2E/真机演练用）
```bash
python3 scripts/k15_build_packs.py --out /tmp/k15/archives
```
- 产出官方线 1.0.0/1.1.0 + 第三方 SDK 样例线 1.0.0/1.1.0，用于「装/切/回滚」链路演练；
- 它**故意**用一个源码目录出多个版本号（版本号在这里是夹具身份，不是发布身份），因此**不过台账校验**，
  产物**不可作为交付物**；出到 `deliver/dampacks*` 目录会被直接拒绝（防止再走错门）。

口径：manifest 原文 sha256 = pluginHash；`plugin/__init__.py` 里的 `PACK_ID`/`PACK_VERSION` 必须同步改，
否则 `/judge` 会 400「插件包与 manifest 不一致」。
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import shutil
import sys
import zipfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import pack_release_guard as guard  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
OFFICIAL = ROOT / "packs" / "plugins" / "ecommerce"
THIRD = ROOT / "tools" / "pack-sdk-python" / "examples" / "ecommerce_py"
OFFICIAL_ID = "com.soujpg.pack.ecommerce"
THIRD_ID = "com.soujpg.pack.ecommerce_py"
# 归档不打测试目录（真机不需要；体积与噪声都省）
SKIP_DIRS = {"tests", "__pycache__"}


def build_archive(dest: Path, src: Path, pack_id: str, version: str, name: str,
                  lock: dict | None = None) -> dict:
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
    content_hash = guard.content_digest(files)
    # 台账校验放在写盘**之前**：违约时连一个字节的产物都不落（避免半成品被当成包发出去）
    if lock is not None:
        verdict = guard.check(lock, pack_id, version, content_hash)
        print(f"guard: {verdict.render()}", file=sys.stderr)
        if not verdict.ok:
            raise SystemExit(
                f"✗ 出包失败：{pack_id}@{version} 未通过发布台账校验（内容 {content_hash[:12]}…）。\n"
                f"  {verdict.message}\n"
                "  处置：改内容就升 manifest.version 与 plugin/__init__.py 的 PACK_VERSION 再出包；"
                "只是重导同内容才用已发布过的版本号。"
            )
    with zipfile.ZipFile(dest, "w", zipfile.ZIP_DEFLATED) as z:
        for rel, blob in files:
            z.writestr(rel, blob)
    return {
        "id": pack_id,
        "version": version,
        "path": str(dest),
        "sha256": hashlib.sha256(dest.read_bytes()).hexdigest(),
        "pluginHash": hashlib.sha256(raw).hexdigest(),
        "contentHash": content_hash,
        "files": len(files),
        "bytes": dest.stat().st_size,
    }


def _reject_deliver_path(out: Path) -> None:
    """夹具产物不许落到交付目录（真机卡 t_d84301f5 出的就是「夹具当交付」）。"""
    parts = [p.lower() for p in out.resolve().parts]
    if "deliver" in parts and any(p.startswith("dampacks") for p in parts):
        raise SystemExit(
            f"✗ 拒绝写入交付目录：{out}\n"
            "  夹具模式（无 --release-version）的产物不是交付物。交付请走：\n"
            "  python3 scripts/k15_build_packs.py --release-version <新版本号> --out <交付路径> --record"
        )


def release(out: Path, version: str, lock_path: Path, record: bool, note: str) -> dict:
    lock = guard.load_lock(lock_path)
    name = f"电商主图合规（官方包 发布 v{version}）"
    info = build_archive(out, OFFICIAL, OFFICIAL_ID, version, name, lock=lock)
    if record:
        guard.record(lock, OFFICIAL_ID, version, info["contentHash"], info["sha256"],
                     note=note or f"k15 release {version}")
        guard.save_lock(lock, lock_path)
        print(f"[recorded] {OFFICIAL_ID}@{version} → {lock_path}", file=sys.stderr)
    return info


def fixtures(out: Path) -> dict:
    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True)
    print(
        "⚠ 夹具模式（test-only）：产物仅供安装/切版本/回滚演练，**不可作为交付物**。\n"
        "  交付请用 --release-version <版本号>（过发布台账硬校验）。",
        file=sys.stderr,
    )
    return {
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


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", required=True, help="发布模式=单个 .dampack 路径；夹具模式=目录")
    ap.add_argument("--release-version", help="发布模式：本次要发布的版本号（必须 > 台账里该 id 的最高版本）")
    ap.add_argument("--lock", default=None, help="台账（默认 packs/pack-content-lock.json）")
    ap.add_argument("--record", action="store_true", help="发布模式：出包成功后把内容摘要写进台账")
    ap.add_argument("--note", default="", help="--record 时记进台账的备注")
    a = ap.parse_args()
    out = Path(a.out)
    lock_path = Path(a.lock) if a.lock else guard.DEFAULT_LOCK

    if a.release_version:
        info = release(out, a.release_version, lock_path, a.record, a.note)
        print(json.dumps(info, ensure_ascii=False, indent=1))
        return 0

    _reject_deliver_path(out)
    result = fixtures(out)
    (out / "archives.json").write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(result, ensure_ascii=False, indent=1))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
