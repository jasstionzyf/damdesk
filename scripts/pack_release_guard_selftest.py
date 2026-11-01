#!/usr/bin/env python3
"""发布守卫的反向验证（卡 t_b5f22e5e 要求 #3）：**「内容变 + 版本不变」必须红**。

跑的不是库函数，是真实出包命令（`k15_build_packs.py` / `plugin_detail_build_pack.py`），
逐条断退出码、产物是否存在、提示语是否含「内容变了必须升 PACK_VERSION」。

用法：python3 scripts/pack_release_guard_selftest.py [--verbose]
退出码：0 全过；1 有用例失败。
不写仓库内任何文件：台账用临时副本，包源码用临时副本（边界：不动包内容与语料）。
"""
from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts"))
import pack_release_guard as guard  # noqa: E402

PY = sys.executable
K15 = ROOT / "scripts" / "k15_build_packs.py"
DETAIL = ROOT / "scripts" / "plugin_detail_build_pack.py"
LOCK = ROOT / "packs" / "pack-content-lock.json"
OLD_110 = Path("~/deliver/dampacks-d944511/com.soujpg.pack.ecommerce-1.1.0.dampack").expanduser()
NEW_110 = Path("~/deliver/dampacks-d56d81e/com.soujpg.pack.ecommerce-1.1.0.dampack").expanduser()
# 旧/新交付包不在本机时（换机器/清理过 deliver 目录），只跳过「与真实历史包互认」的用例，其余照跑
HISTORY = OLD_110.is_file() and NEW_110.is_file()

VERBOSE = False
RESULTS: list[tuple[str, bool, str]] = []


def run(argv: list[str]) -> tuple[int, str]:
    p = subprocess.run([PY, *argv], capture_output=True, text=True, cwd=ROOT)
    out = (p.stdout or "") + (p.stderr or "")
    if VERBOSE:
        print(f"  $ {' '.join(argv)}\n  rc={p.returncode}\n{out.strip()}\n")
    return p.returncode, out


def case(name: str, cond: bool, detail: str) -> None:
    RESULTS.append((name, cond, detail))
    print(f"[{'PASS' if cond else 'FAIL'}] {name}" + (f" — {detail}" if detail and not cond else ""))


def mutated_src(tmp: Path) -> Path:
    """把官方包复制一份并**真的改内容**（改判定阈值注释 + 新增一行常量），模拟「packs/ 有改动」。"""
    src = tmp / "ecommerce-mutated"
    shutil.copytree(ROOT / "packs" / "plugins" / "ecommerce", src)
    rules = src / "resources" / "rules.json"
    data = json.loads(rules.read_text(encoding="utf-8"))
    data["_selftest_marker"] = "contents-changed-but-version-not-bumped"  # 内容变更（规则文件是包内容）
    rules.write_text(json.dumps(data, ensure_ascii=False, indent=2), encoding="utf-8")
    return src


def main() -> int:
    global VERBOSE
    ap = argparse.ArgumentParser()
    ap.add_argument("--verbose", action="store_true")
    VERBOSE = ap.parse_args().verbose
    tmp = Path(tempfile.mkdtemp(prefix="packguard-"))
    try:
        lock = guard.load_lock(LOCK)
        ecom = lock["packs"]["com.soujpg.pack.ecommerce"]
        src_hash = guard.content_digest(guard.source_files(ROOT / "packs" / "plugins" / "ecommerce"))

        # ---- 0 台账本身自证：已发布包能被台账认出来（幂等），说明摘要口径没错位 ----
        if HISTORY:
            rc, out = run([str(ROOT / "scripts" / "pack_release_guard.py"), "check",
                           "--pack", str(OLD_110)])
            case("0a 台账↔旧交付 1.1.0（d944511）互认 = 幂等 PASS", rc == 0 and "idempotent" in out, out.strip())
            rc, out = run([str(ROOT / "scripts" / "pack_release_guard.py"), "check",
                           "--pack", str(NEW_110)])
            case("0b 台账↔d56d81e 重导 1.1.0 = 违约（真机事故原样复现）",
                 rc == 1 and guard.VIOLATION in out and "content-changed" in out, out.strip())
        else:
            print("[SKIP] 0a/0b/4a/4b：本机没有 d944511/d56d81e 交付包，跳过与真实历史包互认的用例")

        # ---- 1 台账自洽：源码要么仍与台账最高版本同内容（同版本重导 = 幂等 PASS），
        #      要么内容已按守卫自己的规矩升版（manifest.version > 台账最高版本；v2.0 契约切换即此态）----
        latest_v = max(ecom, key=lambda v: tuple(int(x) for x in v.split(".")))
        if src_hash == ecom[latest_v]["contentHash"]:
            out1 = tmp / f"same-{latest_v}.dampack"
            rc, out = run([str(K15), "--release-version", latest_v, "--out", str(out1)])
            rc_hash = guard.digest_archive(out1) if out1.is_file() else None
            case(f"1 同内容 + 同版本（{latest_v}）重导 = 幂等 PASS",
                 rc == 0 and rc_hash == ecom[latest_v]["contentHash"] and "idempotent" in out, out.strip())
        else:
            mv = json.loads((ROOT / "packs" / "plugins" / "ecommerce" / "manifest.json")
                            .read_text(encoding="utf-8"))["version"]
            bumped = (tuple(int(x) for x in mv.split("."))
                      > tuple(int(x) for x in latest_v.split(".")))
            case(f"1 源码内容已前进（≠ 台账 {latest_v} 入账）→ manifest 必须已升版（{mv} > {latest_v}）",
                 bumped, f"manifest.version={mv}, ledger max={latest_v}, src={src_hash[:12]}…")

        # ---- 2 内容变 + 版本不变 = 出包必须红，且不落产物 ----
        out2 = tmp / "changed-1.1.1.dampack"
        rc, out = run([str(DETAIL), "--src", str(mutated_src(tmp)), "--id", "com.soujpg.pack.ecommerce",
                       "--version", "1.1.1", "--out", str(out2)])
        case("2 内容变 + 同版本 → 出包失败且提示升版本",
             rc != 0 and guard.VIOLATION in out and not out2.exists(), out.strip())

        # ---- 2b 同一件事换 k15 发布模式（官方包真出包路径）也必须红 ----
        bad_lock = tmp / "lock-pretend-1.1.1-published-with-new-content.json"
        fib = json.loads(LOCK.read_text(encoding="utf-8"))
        fib["packs"]["com.soujpg.pack.ecommerce"]["1.1.1"]["contentHash"] = ecom["1.1.0"]["contentHash"]
        bad_lock.write_text(json.dumps(fib, ensure_ascii=False, indent=2), encoding="utf-8")
        out2b = tmp / "k15-changed-1.1.1.dampack"
        rc, out = run([str(K15), "--release-version", "1.1.1", "--out", str(out2b), "--lock", str(bad_lock)])
        case("2b k15 发布模式：内容已变、版本仍是已发布过的 1.1.1 → 出包失败",
             rc != 0 and guard.VIOLATION in out and not out2b.exists(), out.strip())

        # ---- 3 内容变 + 升版本 = 放行，并入账后可幂等重导 ----
        rec_lock = tmp / "lock-record.json"
        shutil.copyfile(LOCK, rec_lock)
        out3 = tmp / "changed-1.1.2.dampack"
        rc, out = run([str(K15), "--release-version", "1.1.2", "--out", str(out3),
                       "--lock", str(rec_lock), "--record", "--note", "selftest"])
        rec = json.loads(rec_lock.read_text(encoding="utf-8"))["packs"]["com.soujpg.pack.ecommerce"]
        out3b = tmp / "changed-1.1.2-again.dampack"
        rc2, out2 = run([str(K15), "--release-version", "1.1.2", "--out", str(out3b), "--lock", str(rec_lock)])
        case("3 内容变了但升到 1.1.2 → 放行 + 入账 + 再重导幂等",
             rc == 0 and out3.is_file() and "1.1.2" in rec and rc2 == 0 and "idempotent" in out2,
             (out + out2).strip())

        # ---- 3b 版本回退/复用旧版本号也不行 ----
        out3c = tmp / "rollback-1.0.9.dampack"
        rc, out = run([str(K15), "--release-version", "1.0.9", "--out", str(out3c)])
        case("3b 新号但低于已发布最高版本 → 拒绝（version-regression）",
             rc != 0 and "version-regression" in out and not out3c.exists(), out.strip())

        # ---- 4 --prev 互查（离线/无台账场景）----
        if HISTORY:
            empty_lock = tmp / "lock-empty.json"
            empty_lock.write_text(json.dumps({"schema": 1, "algorithm": guard.ALGORITHM, "packs": {}}),
                                  encoding="utf-8")
            same_out = tmp / "prev-same.dampack"
            rc, out = run([str(DETAIL), "--src", "packs/plugins/ecommerce", "--id", "com.soujpg.pack.ecommerce",
                           "--version", "1.1.0", "--out", str(same_out), "--lock", str(empty_lock),
                           "--prev", str(NEW_110)])
            case("4a --prev 与本次同版本且内容相同 → 放行",
                 rc == 0 and same_out.is_file() and "内容相同" in out, out.strip())
            diff_out = tmp / "prev-diff.dampack"
            rc, out = run([str(DETAIL), "--src", "packs/plugins/ecommerce", "--id", "com.soujpg.pack.ecommerce",
                           "--version", "1.1.0", "--out", str(diff_out), "--lock", str(empty_lock),
                           "--prev", str(OLD_110)])
            case("4b --prev 同版本内容不同（旧 1.1.0 vs 现源码）→ 失败",
                 rc != 0 and guard.VIOLATION in out and not diff_out.exists(), out.strip())

        # ---- 5 夹具模式不许落交付目录；手验副本（新 id）不受台账影响 ----
        rc, out = run([str(K15), "--out", str(tmp / "deliver" / "dampacks-selftest")])
        case("5a 夹具模式拒绝写入 deliver/dampacks* 目录",
             rc != 0 and "拒绝写入交付目录" in out and not (tmp / "deliver" / "dampacks-selftest").exists(),
             out.strip())
        copy_out = tmp / "ecommerce_ui-1.3.0.dampack"
        rc, out = run([str(DETAIL), "--src", "packs/plugins/ecommerce", "--id",
                       "com.soujpg.pack.ecommerce_ui", "--version", "1.3.0", "--out", str(copy_out)])
        case("5b 手验副本（换 id 同源码，现有 E2E 用法）照旧放行",
             rc == 0 and copy_out.is_file() and "new-version" in out, out.strip())
        same_id = tmp / "ecommerce-1.3.0-copy-same-source.dampack"
        rc, out = run([str(DETAIL), "--src", "packs/plugins/ecommerce", "--id",
                       "com.soujpg.pack.ecommerce", "--version", "1.3.0", "--out", str(same_id)])
        case("5c 官方 id 直接用现源码打 1.3.0（> 台账最高 1.1.1）→ 放行（repro_mix_results 用法）",
             rc == 0 and same_id.is_file(), out.strip())
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    failed = [n for n, ok, _ in RESULTS if not ok]
    print(f"\n{len(RESULTS) - len(failed)}/{len(RESULTS)} PASS" + (f"  失败: {failed}" if failed else ""))
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
