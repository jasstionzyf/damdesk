#!/usr/bin/env python3
"""dampack 发布守卫：**同 id@version 的内容必须字节稳定**，改内容必须升 PACK_VERSION。

为什么必须有（卡 t_b5f22e5e，真机卡 t_d84301f5 实锤）：
出包机器上「重导 dampack」时 `packs/` 内容变了、版本号没升（仍 1.1.0），装上机器的
旧 1.1.0 于是装不进新包 —— pack-host 按协议 §7 铁律 7 硬拒（409 `pack_version_conflict`
「改内容必须升 version」），而内置包 `pack_builtin_protected` 不可卸载 → **任何装过旧版本的
机器永远升不上去**。宿主的拒装是对的（保护在），错的是出包流程没拦住「同版本换内容」。

守卫口径（`dampack-content-v1`）：
  内容 = 归档里**除 manifest.json 之外**的全部文件；`plugin/__init__.py` 里的
  `PACK_ID = "..."` / `PACK_VERSION = "..."` 两行在计算前抹掉（构建器会按 id/version 改写这两行，
  它们属于「包身份」，不属于「包内容」）。逐条 `sha256(路径\\0文件摘要)` 排序后再取总摘要。

三条判定（lock = `packs/pack-content-lock.json`，记录**已发布**过的 version→内容摘要）：
  1. version 已在 lock 且内容相同 → PASS（同版本重导 = 幂等，允许）
  2. version 已在 lock 但内容不同 → **FAIL**「内容变了必须升 PACK_VERSION」
  3. version 不在 lock → 新版本，必须严格大于该 id 已发布的最高版本；否则 FAIL（版本回退）

用法：
  # 看一个归档/源码目录的内容摘要
  python3 scripts/pack_release_guard.py digest --pack /path/x.dampack
  python3 scripts/pack_release_guard.py digest --src packs/plugins/ecommerce

  # 出包前互查（--pack 传刚打出来的包 / --prev 传上一发布版本，二者都可）
  python3 scripts/pack_release_guard.py check --pack /tmp/x-1.1.0.dampack \
      --id com.soujpg.pack.ecommerce --version 1.1.0
  python3 scripts/pack_release_guard.py check --pack /tmp/x-1.1.1.dampack --prev ~/deliver/dampacks-d944511/com.soujpg.pack.ecommerce-1.1.0.dampack

  # 发布成功后登记进 lock（--version/--id 缺省取归档 manifest）
  python3 scripts/pack_release_guard.py record --pack /tmp/x-1.1.1.dampack --note "d56d81e 同源 1.1.1"

退出码：0 通过（可能带 WARN）；1 违约；2 用法/lock 损坏。
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import zipfile
from dataclasses import dataclass
from pathlib import Path

ALGORITHM = "dampack-content-v1"
ROOT = Path(__file__).resolve().parent.parent
DEFAULT_LOCK = ROOT / "packs" / "pack-content-lock.json"
VIOLATION = "内容变了必须升 PACK_VERSION"
# 构建器视为「不归档」的目录（fixture/临时产物），digest --src 时同样跳过
SKIP_DIRS = {"tests", "__pycache__"}
# 包身份行：由构建器按 id/version 改写，不进内容摘要
IDENTITY_RE = re.compile(r"^PACK_(?:ID|VERSION)\s*=.*$", re.M)


class GuardError(RuntimeError):
    """用法/lock 类错误（exit 2）。"""


# ---------------------------------------------------------------- 摘要


def normalize_entry(rel: str, blob: bytes) -> bytes:
    """把「包身份」抹掉，只留「包内容」。"""
    if rel == "plugin/__init__.py":
        try:
            text = blob.decode("utf-8")
        except UnicodeDecodeError:  # 非 utf-8 的 __init__.py 不该存在，按原字节算
            return blob
        return IDENTITY_RE.sub("", text).encode("utf-8")
    return blob


def content_digest(files: list[tuple[str, bytes]]) -> str:
    """files = [(相对路径, 字节)]，可含 manifest.json（会被忽略）。"""
    items: list[tuple[str, str]] = []
    for rel, blob in files:
        if rel == "manifest.json" or rel.endswith("/"):
            continue
        items.append((rel, hashlib.sha256(normalize_entry(rel, blob)).hexdigest()))
    items.sort()
    h = hashlib.sha256()
    for rel, digest in items:
        h.update(f"{rel}\0{digest}\n".encode())
    return h.hexdigest()


def read_archive(path: Path) -> tuple[dict, list[tuple[str, bytes]]]:
    if not path.is_file():
        raise GuardError(f"归档不存在：{path}")
    try:
        with zipfile.ZipFile(path) as z:
            if "manifest.json" not in z.namelist():
                raise GuardError(f"归档缺 manifest.json（不是 dampack？）：{path}")
            manifest = json.loads(z.read("manifest.json").decode("utf-8"))
            files = [(n, z.read(n)) for n in z.namelist() if not n.endswith("/")]
    except zipfile.BadZipFile as e:
        raise GuardError(f"不是合法 zip：{path}（{e}）") from e
    return manifest, files


def digest_archive(path: Path) -> str:
    return content_digest(read_archive(path)[1])


def source_files(src: Path) -> list[tuple[str, bytes]]:
    if not (src / "manifest.json").is_file():
        raise GuardError(f"源码目录缺 manifest.json：{src}")
    out: list[tuple[str, bytes]] = []
    for p in sorted(src.rglob("*")):
        if not p.is_file():
            continue
        rel = p.relative_to(src).as_posix()
        if any(part in SKIP_DIRS for part in rel.split("/")) or rel.endswith(".pyc"):
            continue
        out.append((rel, p.read_bytes()))
    return out


# ---------------------------------------------------------------- lock


def load_lock(path: Path = DEFAULT_LOCK) -> dict:
    if not path.exists():
        return {"schema": 1, "algorithm": ALGORITHM, "packs": {}}
    try:
        lock = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as e:
        raise GuardError(f"lock 不是合法 JSON：{path}（{e}）") from e
    if lock.get("algorithm") != ALGORITHM:
        raise GuardError(
            f"lock 算法版本不符：{path} 里是 {lock.get('algorithm')!r}，本脚本是 {ALGORITHM!r}"
            "（换算法等于换口径，必须整份重算而不是混用）"
        )
    lock.setdefault("packs", {})
    return lock


def save_lock(lock: dict, path: Path = DEFAULT_LOCK) -> None:
    lock["schema"] = 1
    lock["algorithm"] = ALGORITHM
    lock["packs"] = {pid: dict(sorted(vs.items(), key=lambda kv: version_key(kv[0])))
                     for pid, vs in sorted(lock["packs"].items())}
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(lock, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


# ---------------------------------------------------------------- 版本比较（对齐 crates/pack-host/src/registry.rs::compare_versions）


def version_key(v: str) -> tuple:
    key = []
    for seg in re.split(r"[.\-+]", v):
        key.append((0, int(seg), "") if seg.isdigit() else (1, 0, seg))
    return tuple(key)


def compare_versions(a: str, b: str) -> int:
    ka, kb = version_key(a), version_key(b)
    for i in range(max(len(ka), len(kb))):
        x = ka[i] if i < len(ka) else None
        y = kb[i] if i < len(kb) else None
        if x is None:
            return -1
        if y is None:
            return 1
        if x != y:
            return -1 if x < y else 1
    return 0


# ---------------------------------------------------------------- 判定


@dataclass
class Verdict:
    ok: bool
    kind: str          # idempotent | same-content | new-version | content-changed | version-regression
    message: str
    lock_hash: str | None = None
    latest_version: str | None = None

    def render(self) -> str:
        tag = "PASS" if self.ok else "FAIL"
        return f"[{tag}] {self.kind}: {self.message}"


def check(lock: dict, pack_id: str, version: str, content_hash: str) -> Verdict:
    published = lock.get("packs", {}).get(pack_id, {})
    latest = max(published, key=version_key) if published else None
    if version in published:
        if published[version].get("contentHash") == content_hash:
            return Verdict(True, "idempotent",
                           f"{pack_id}@{version} 内容与已发布版本一致（{content_hash[:12]}…）→ 允许同版本重导",
                           content_hash, latest)
        return Verdict(
            False, "content-changed",
            f"{pack_id}@{version} 内容变了（已发布 {published[version].get('contentHash', '?')[:12]}… → "
            f"本次 {content_hash[:12]}…）：**{VIOLATION}**。已装机器的升级姿势 = 新版本号 + activate"
            "（内置包不可卸载，同版本装不上）。",
            published[version].get("contentHash"), latest,
        )
    # 新版本
    if latest is not None and compare_versions(version, latest) <= 0:
        return Verdict(False, "version-regression",
                       f"{pack_id}@{version} 不是新版本（已发布最高 {latest}）——要么升版本号，要么别改内容",
                       None, latest)
    same = [v for v, e in published.items() if e.get("contentHash") == content_hash]
    if same:
        return Verdict(True, "new-version",
                       f"{pack_id}@{version} 是新版本，内容与已发布的 {', '.join(sorted(same, key=version_key))} 完全相同"
                       "（WARN：没必要升版本，但允许）",
                       None, latest)
    return Verdict(True, "new-version",
                   f"{pack_id}@{version} 是新版本（内容 {content_hash[:12]}…，已发布最高 {latest or '无'}）→ 允许",
                   None, latest)


def record(lock: dict, pack_id: str, version: str, content_hash: str,
           archive_sha256: str | None = None, note: str = "", source: str = "") -> dict:
    entry = {"contentHash": content_hash}
    if archive_sha256:
        entry["archiveSha256"] = archive_sha256
    if source:
        entry["source"] = source
    if note:
        entry["note"] = note
    lock.setdefault("packs", {}).setdefault(pack_id, {})[version] = entry
    return entry


# ---------------------------------------------------------------- CLI


def _resolve(args) -> tuple[str, str, str, Path | None]:
    """返回 (pack_id, version, content_hash, 归档路径或 None)。"""
    if args.pack:
        path = Path(args.pack)
        manifest, files = read_archive(path)
        pack_id = args.id or manifest.get("id")
        version = args.version or manifest.get("version")
        if not pack_id or not version:
            raise GuardError("归档 manifest 缺 id/version，且命令行没给 --id/--version")
        return pack_id, version, content_digest(files), path
    if args.src:
        src = Path(args.src)
        manifest = json.loads((src / "manifest.json").read_text(encoding="utf-8"))
        if not args.id or not args.version:
            raise GuardError("--src 模式必须显式给 --id 与 --version（源码 manifest 的 id/version 不是出包用的身份）")
        return args.id, args.version, content_digest(source_files(src)), None
    raise GuardError("必须给 --pack 或 --src")


def cmd_digest(args) -> int:
    if args.pack:
        pid, ver, ch, path = _resolve(args)
        print(json.dumps({"id": pid, "version": ver, "contentHash": ch, "pack": str(path)},
                         ensure_ascii=False))
    else:
        if not args.src:
            raise GuardError("必须给 --pack 或 --src")
        src = Path(args.src)
        files = source_files(src)
        print(json.dumps({"src": str(src), "contentHash": content_digest(files),
                          "files": len([1 for r, _ in files if r != "manifest.json"])},
                         ensure_ascii=False))
    return 0


def cmd_check(args) -> int:
    lock_path = Path(args.lock) if args.lock else DEFAULT_LOCK
    lock = load_lock(lock_path)
    pid, ver, ch, path = _resolve(args)
    print(f"guard: {pid}@{ver} 内容 {ch[:16]}…  (lock={lock_path})")
    v = check(lock, pid, ver, ch)
    print(v.render())
    if args.prev:
        prev_path = Path(args.prev)
        prev_manifest, prev_files = read_archive(prev_path)
        prev_ch = content_digest(prev_files)
        prev_ver = prev_manifest.get("version", "?")
        print(f"guard: --prev {prev_path.name} = {prev_manifest.get('id')}@{prev_ver} 内容 {prev_ch[:16]}…")
        if prev_ch != ch:
            # 只有「同一个 version 换了内容」才是违约；跨版本内容变化本来就是升级的本意
            if prev_ver == ver:
                print(f"[FAIL] content-changed: --prev 与本次同版本（{ver}）但内容不同："
                      f"{prev_ch[:12]}… → {ch[:12]}…：**{VIOLATION}**")
                return 1
            print(f"[warn] 内容相对上一版本有变（{prev_ver} → {ver}），版本号已升 → 合规")
        else:
            print("[warn] 内容与 --prev 完全一致（同版本重导=幂等；换了版本号则属无意义升版）")
    return 0 if v.ok else 1


def cmd_record(args) -> int:
    lock_path = Path(args.lock) if args.lock else DEFAULT_LOCK
    lock = load_lock(lock_path)
    if not args.pack:
        raise GuardError("record 只接受 --pack <已出好的 .dampack>")
    pid, ver, ch, path = _resolve(args)
    assert path is not None
    entry = record(lock, pid, ver, ch, hashlib.sha256(path.read_bytes()).hexdigest(),
                   note=args.note, source=args.source)
    save_lock(lock, lock_path)
    print(json.dumps({"recorded": f"{pid}@{ver}", **entry}, ensure_ascii=False))
    return 0


def cmd_list(args) -> int:
    lock = load_lock(Path(args.lock) if args.lock else DEFAULT_LOCK)
    for pid, versions in lock["packs"].items():
        print(pid)
        for ver, e in versions.items():
            print(f"  {ver:<8} {e.get('contentHash', '?')[:16]}…  {e.get('note', '')}")
    if not lock["packs"]:
        print("（lock 为空）")
    return 0


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("digest", "check", "record", "list"):
        sp = sub.add_parser(name)
        sp.add_argument("--pack", help="已打好的 .dampack")
        sp.add_argument("--src", help="包源码目录（未出包时自检）")
        sp.add_argument("--id")
        sp.add_argument("--version")
        sp.add_argument("--prev", help="上一发布版本的 .dampack（互查）")
        sp.add_argument("--lock", help=f"lock 文件（默认 {DEFAULT_LOCK.relative_to(ROOT)}）")
        sp.add_argument("--note")
        sp.add_argument("--source")
    args = ap.parse_args(argv)
    try:
        return {"digest": cmd_digest, "check": cmd_check, "record": cmd_record, "list": cmd_list}[args.cmd](args)
    except GuardError as e:
        print(f"[ERROR] {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
