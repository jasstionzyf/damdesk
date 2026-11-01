#!/usr/bin/env python3
"""把仓库里的样例插件打成一个 `.dampack`（zip 根目录有 manifest.json），改 id/name/version。

为什么需要：N4 手验要演示「装一个插件 → 去它的详情页配规格」。仓库自带的电商包是**内置包**
（随产品分发、不进账本、不可卸载），拿它演示不了安装；所以复制一份换个 id 装进来。
做法与 `scripts/pack_ui_e2e.py::build_archive` 一致（同一份插件代码、两个包身份），
并且**同步改 plugin/__init__.py 里的 PACK_ID/PACK_VERSION**——只改 manifest 不改常量，
宿主握手会以 `插件包与 manifest 不一致` 拒掉（K10 实测踩过）。

**出包前硬校验**（同 `scripts/k15_build_packs.py --release-version`）：拿
`packs/pack-content-lock.json`（已发布台账）比内容摘要 —— 同 id@version 内容变了就直接失败
（宿主编译期会 409 `pack_version_conflict`，内置包还卸不掉 → 装过旧版的机器永远升级不了；
真机卡 t_d84301f5 实锤）。内容没变 = 同版本重导，幂等放行。

用法：
  python3 scripts/plugin_detail_build_pack.py \\
      --src packs/plugins/ecommerce \\
      --out /tmp/n4/archives/com.soujpg.pack.ecommerce_ui-1.3.0.dampack \\
      --id com.soujpg.pack.ecommerce_ui --version 1.3.0 --name "电商主图合规（手验副本）"

  # 不加台账、只跟「上一发布的包」互查（临时/离线场景）
  python3 scripts/plugin_detail_build_pack.py --src packs/plugins/ecommerce \\
      --out /tmp/x-1.1.1.dampack --id com.soujpg.pack.ecommerce --version 1.1.1 \\
      --prev ~/deliver/dampacks-d944511/com.soujpg.pack.ecommerce-1.1.0.dampack

  # 发布成功后入账（只对交付包做，手验副本别入账）
  ... --version 1.1.2 --out ~/deliver/dampacks-<sha>/... --record --note "d56d81e+..."

`--no-guard` 只给「明确知道自己在造重复内容且不进台账」的一次性脚本留的逃生口，会在 stderr 显著告警。
"""
import argparse
import hashlib
import json
import re
import sys
import zipfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import pack_release_guard as guard  # noqa: E402

# 归档不打测试目录（与 k15_build_packs.py 同一口径：真机/发布不需要 tests，且两个构建器出的包必须内容可比）
SKIP_DIRS = {"tests", "__pycache__"}


def build(src: Path, dest: Path, pack_id: str, version: str, name: str,
          lock: dict | None = None, prev: Path | None = None) -> dict:
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
        if rel == "manifest.json" or any(part in SKIP_DIRS for part in rel.split("/")) or rel.endswith(".pyc"):
            continue
        blob = p.read_bytes()
        if rel == "plugin/__init__.py":
            text = blob.decode("utf-8")
            text = re.sub(r'^PACK_ID = ".*"$', f'PACK_ID = "{pack_id}"', text, flags=re.M)
            text = re.sub(r'^PACK_VERSION = ".*"$', f'PACK_VERSION = "{version}"', text, flags=re.M)
            blob = text.encode("utf-8")
        files.append((rel, blob))
    content_hash = guard.content_digest(files)

    # ---- 硬校验（在写盘之前：违约时一个字节的产物都不落）----
    if lock is not None:
        verdict = guard.check(lock, pack_id, version, content_hash)
        print(f"guard: {verdict.render()}", file=sys.stderr)
        if not verdict.ok:
            raise SystemExit(
                f"✗ 出包失败：{pack_id}@{version} 未通过发布台账校验（内容 {content_hash[:12]}…）。\n"
                f"  {verdict.message}\n"
                "  处置：内容改了就把 --version 升上去（同时改 manifest 与 PACK_VERSION），"
                "内容没改才允许同版本重导。"
            )
    if prev is not None:
        prev_manifest, prev_files = guard.read_archive(prev)
        prev_hash = guard.content_digest(prev_files)
        prev_ver = prev_manifest.get("version", "?")
        print(f"guard: --prev {Path(prev).name} = {prev_manifest.get('id')}@{prev_ver} "
              f"内容 {prev_hash[:16]}…", file=sys.stderr)
        if prev_hash != content_hash and prev_ver == version:
            raise SystemExit(
                f"✗ 出包失败：与 --prev 同版本（{version}）但内容不同"
                f"（{prev_hash[:12]}… → {content_hash[:12]}…）。\n"
                f"  {guard.VIOLATION}：改内容就升版本号；同内容重导才允许同版本。"
            )
        print(f"guard: 与 --prev 比对通过（{'内容相同' if prev_hash == content_hash else f'内容变且版本已升 {prev_ver}→{version}'}）",
              file=sys.stderr)

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
    }


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--src", default="packs/plugins/ecommerce")
    ap.add_argument("--out", required=True)
    ap.add_argument("--id", required=True)
    ap.add_argument("--version", default="1.3.0")
    ap.add_argument("--name", default="电商主图合规（手验副本）")
    ap.add_argument("--lock", default=None, help="发布台账（默认 packs/pack-content-lock.json）")
    ap.add_argument("--prev", default=None, help="上一发布版本的 .dampack（互查内容是否变了）")
    ap.add_argument("--record", action="store_true", help="出包成功后写入台账（只对交付包用）")
    ap.add_argument("--note", default="", help="--record 的备注")
    ap.add_argument("--no-guard", action="store_true", help="跳过台账校验（一次性脚本专用，会告警）")
    a = ap.parse_args()

    lock_path = Path(a.lock) if a.lock else guard.DEFAULT_LOCK
    lock = None
    if a.no_guard:
        print("⚠ --no-guard：跳过发布台账校验，产物不得作为交付物", file=sys.stderr)
    else:
        lock = guard.load_lock(lock_path)

    info = build(Path(a.src), Path(a.out), a.id, a.version, a.name,
                 lock=lock, prev=Path(a.prev) if a.prev else None)
    if a.record:
        if a.no_guard:
            raise SystemExit("✗ --record 与 --no-guard 互斥（不校验的内容不许进台账）")
        assert lock is not None
        guard.record(lock, a.id, a.version, info["contentHash"], info["sha256"],
                     note=a.note or f"plugin_detail_build_pack {a.version}")
        guard.save_lock(lock, lock_path)
        print(f"[recorded] {a.id}@{a.version} → {lock_path}", file=sys.stderr)
    print(json.dumps(info, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
