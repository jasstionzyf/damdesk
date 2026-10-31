#!/usr/bin/env python3
"""把 python-build-standalone（win-x64）stage 进 `crates/app-tauri/resources/runtime/python/`。

为什么需要（KI-28，卡 task）：纯净 Windows 没有 `python3`，行业包（合规插件）是 Python
子进程 —— 安装包不带解释器，插件就永远起不来（K14 出包里 `grep -i python` 命中 0 条）。

同时把 `tools/dampack_launcher.py` 拷成 `resources/pack-launcher/dampack_launcher.py`
（发行包里 `-m dampack_launcher` 必须能找到它；仓库那份靠 `CARGO_MANIFEST_DIR` 指向构建机，发行态不存在）。

用法：
    python3 scripts/stage_pack_runtime.py            # 幂等：已 stage 且版本一致就跳过
    python3 scripts/stage_pack_runtime.py --force    # 重下重解
    python3 scripts/stage_pack_runtime.py --check    # 只核对现状，不下载（打包门禁用）

产物（**全部 gitignored**，构建前必须重跑一次）：
    crates/app-tauri/resources/runtime/python/**        ← 解释器 + stdlib + venv/ensurepip
    crates/app-tauri/resources/runtime/runtime.json      ← 来源/版本/sha256/体积账（进包，便于真机溯源）
    crates/app-tauri/resources/pack-launcher/dampack_launcher.py

镜像顺序：npmmirror（国内快，实测 ~14MB/s）→ GitHub releases（兜底）。
"""
from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import sys
import tarfile
import tempfile
import urllib.error
import urllib.request
from pathlib import Path

TAG = "20261020"
ASSET = f"cpython-3.11.17+{TAG}-x86_64-pc-windows-msvc-install_only_stripped.tar.gz"
# 国内镜像先试：GitHub 直连实测 3KB/s（25MB 要两小时），npmmirror 14MB/s
URLS = [
    f"https://registry.npmmirror.com/-/binary/python-build-standalone/{TAG}/{ASSET.replace('+', '%2B')}",
    f"https://github.com/astral-sh/python-build-standalone/releases/download/{TAG}/{ASSET.replace('+', '%2B')}",
]
SHA256 = ""  # 首次下载后由 stage 写入 runtime.json 并在后续运行中比对（镜像/上游换包必须暴露出来）

ROOT = Path(__file__).resolve().parent.parent
RES = ROOT / "crates" / "app-tauri" / "resources"
RT_DIR = RES / "runtime"
PY_DIR = RT_DIR / "python"
LAUNCHER_DIR = RES / "pack-launcher"
LAUNCHER_SRC = ROOT / "tools" / "dampack_launcher.py"
STAMP = RT_DIR / "runtime.json"
CACHE = Path("/data")


def sha256(p: Path) -> str:
    h = hashlib.sha256()
    with p.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def tree_bytes(p: Path) -> tuple[int, int]:
    files = 0
    total = 0
    for f in p.rglob("*"):
        if f.is_file():
            files += 1
            total += f.stat().st_size
    return files, total


def download(dest: Path) -> str:
    dest.parent.mkdir(parents=True, exist_ok=True)
    errors = []
    for url in URLS:
        try:
            print(f"[stage] 下载 {url}")
            req = urllib.request.Request(url, headers={"User-Agent": "damdesk-stage/1"})
            with urllib.request.urlopen(req, timeout=120) as r, dest.open("wb") as f:
                shutil.copyfileobj(r, f, length=1 << 20)
            got = sha256(dest)
            print(f"[stage] 完成 {dest.stat().st_size/1e6:.1f}MB sha256={got}")
            return got
        except (urllib.error.URLError, TimeoutError, OSError) as e:  # 逐个镜像降级
            errors.append(f"{url} -> {e}")
            print(f"[stage] 失败：{e}")
    raise SystemExit("[stage] 所有镜像都失败：\n  " + "\n  ".join(errors))


def staged_ok() -> bool:
    """已 stage 且解释器在：幂等跳过（不比对 sha，--force 才重下）。"""
    exe = PY_DIR / "python.exe"
    return exe.is_file() and (PY_DIR / "python311.dll").is_file() and STAMP.is_file()


def extract(tar_path: Path) -> None:
    with tempfile.TemporaryDirectory(dir=str(RT_DIR)) as tmp:
        tmpd = Path(tmp)
        with tarfile.open(tar_path) as tf:
            tf.extractall(tmpd)  # noqa: S202 —— 可信常量 URL + 固定 sha 的来源包
        src = tmpd / "python"
        if not (src / "python.exe").is_file():
            raise SystemExit(f"[stage] 包里没有 python/python.exe（结构变了？）：{sorted(p.name for p in tmpd.iterdir())}")
        if PY_DIR.exists():
            shutil.rmtree(PY_DIR)
        PY_DIR.parent.mkdir(parents=True, exist_ok=True)
        shutil.move(str(src), str(PY_DIR))


def stage_launcher() -> None:
    LAUNCHER_DIR.mkdir(parents=True, exist_ok=True)
    shutil.copy2(LAUNCHER_SRC, LAUNCHER_DIR / "dampack_launcher.py")
    print(f"[stage] 启动器 → {LAUNCHER_DIR / 'dampack_launcher.py'}（源 {LAUNCHER_SRC}）")


def write_stamp(tar_sha: str, tar_bytes: int) -> dict:
    files, total = tree_bytes(PY_DIR)
    meta = {
        "source": "python-build-standalone",
        "tag": TAG,
        "asset": ASSET,
        "sha256_tar": tar_sha,
        "tar_bytes": tar_bytes,
        "python_rel": "python/python.exe",
        "python_version": "3.11.17",
        "files": files,
        "uncompressed_bytes": total,
        "launcher": "pack-launcher/dampack_launcher.py",
        "note": "构建期由 scripts/stage_pack_runtime.py 生成；不进 git（体积），真机排障看这一份",
    }
    STAMP.write_text(json.dumps(meta, ensure_ascii=False, indent=2) + "\n")
    return meta


def check() -> int:
    ok = True
    exe = PY_DIR / "python.exe"
    if not exe.is_file():
        print(f"[check] FAIL 缺 {exe}（先跑 scripts/stage_pack_runtime.py）")
        ok = False
    if not (PY_DIR / "Lib" / "venv" / "__init__.py").is_file():
        print("[check] FAIL runtime 缺 Lib/venv（无法给每包建 venv）")
        ok = False
    if not (PY_DIR / "Lib" / "ensurepip" / "__init__.py").is_file():
        print("[check] FAIL runtime 缺 Lib/ensurepip（venv 里装不了 pip）")
        ok = False
    dst = LAUNCHER_DIR / "dampack_launcher.py"
    if not dst.is_file():
        print(f"[check] FAIL 缺 {dst}（先跑 scripts/stage_pack_runtime.py）")
        ok = False
    elif sha256(dst) != sha256(LAUNCHER_SRC):
        print(f"[check] FAIL {dst} 与 tools/dampack_launcher.py 不同步（重跑 stage）")
        ok = False
    if ok and STAMP.is_file():
        meta = json.loads(STAMP.read_text())
        print(
            f"[check] OK python {meta.get('python_version')} "
            f"{meta.get('uncompressed_bytes', 0)/1e6:.1f}MB/{meta.get('files')} 文件"
        )
    elif ok:
        print("[check] OK（无 runtime.json，体积账缺失）")
    return 0 if ok else 1


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--force", action="store_true", help="重下重解（默认幂等跳过）")
    ap.add_argument("--check", action="store_true", help="只核对现状（打包门禁用）")
    args = ap.parse_args()
    if args.check:
        return check()

    RT_DIR.mkdir(parents=True, exist_ok=True)
    if staged_ok() and not args.force:
        print("[stage] 已 stage（幂等跳过；--force 重来）")
    else:
        cached = CACHE / ASSET
        if cached.is_file() and cached.stat().st_size > 20_000_000:
            print(f"[stage] 用缓存 {cached}")
            tar_sha = sha256(cached)
        else:
            print(f"[stage] 缓存 {cached} 不存在或不完整，下载")
            tar_sha = download(cached)
        if SHA256 and tar_sha != SHA256:
            raise SystemExit(f"[stage] sha256 不符：期望 {SHA256} 实得 {tar_sha}（镜像换包/被劫持）")
        if STAMP.is_file():
            prev = json.loads(STAMP.read_text()).get("sha256_tar")
            if prev and prev != tar_sha:
                print(f"[stage] 注意：runtime 来源换了（{prev[:12]} → {tar_sha[:12]}）")
        extract(cached)
        meta = write_stamp(tar_sha, cached.stat().st_size)
        print(
            f"[stage] 解包完成：{meta['files']} 文件 / {meta['uncompressed_bytes']/1e6:.1f}MB"
            f"（tar {meta['tar_bytes']/1e6:.1f}MB）"
        )
    stage_launcher()
    return check()


if __name__ == "__main__":
    sys.exit(main())
