#!/usr/bin/env python3
"""旅程用例反向验证（卡 t_c117a719 验收 #2）：破坏被测保护 → 用例必须 FAIL 且带阶段名。

用法（建议经 wrapper 跑，自动切容器）：
    python3 scripts/journey_sabotage.py           # 三个破坏点全跑
    python3 scripts/journey_sabotage.py --case 1   # 只跑某个破坏点

判据（脚本自己判定，不靠人眼）：
  1. 打补丁成功（锚点必须唯一命中，否则直接判「破坏脚本自身坏了」）；
  2. 对应用例**必须失败**；
  3. 失败输出里必须出现**预期阶段名**（如 `[S2 管线]`）——只红不带阶段名 = 定位不合格；
  4. 无论成败都还原源码，并校验 sha256 与破坏前一致。

为什么需要它：断言写歪了也会永远绿。只有「拿掉保护必须红」才能证明断言盯着的是
被测行为，而不是自己的实现细节。
"""

import argparse
import hashlib
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# (名字, 文件, 锚点, 替身, cargo test 过滤, 预期阶段名)
CASES = [
    (
        "tag 表写入被注释",
        "crates/svc-pipeline/src/lib.rs",
        "            inserted += stmt.execute(rusqlite::params![asset_id, name, model, t.score])?;",
        "            let _ = (name, t.score); // SABOTAGE: 不写 tag 表",
        "journey_1",
        "S2 管线",
    ),
    (
        "彻底删除跳过向量清理",
        "crates/svc-library/src/lib.rs",
        # KI-11 合并后 purge 不再自己拼 SQL，改走 core-storage 的单一真源
        # delete_resources；破坏锚点跟着实现走（卡 t_79c7b1d8 并入主干时修）
        "                rep.vectors += core_storage::vecstore::delete_resources(&tx, &model, &rids)?;",
        "                let _ = (&model, &rids); // SABOTAGE: 不清向量",
        "journey_5",
        ("I2 彻底删", "I4 不可查"),  # I2 命中删除回执、I4 命中「查不到」——任一即可
    ),
    (
        "入队幂等守卫被移除",
        "crates/core-storage/src/custom_tasks.rs",
        "         WHERE NOT EXISTS (\n"
        "             SELECT 1 FROM job_queue j\n"
        "             WHERE j.kind = ?1 AND j.targetId = ?2 AND j.payload = ?3\n"
        "               AND j.status != 'done'\n"
        "         )",
        "         -- SABOTAGE: 去掉幂等守卫",
        "journey_4",
        "C1 双任务入队",
    ),
]


def sha(p: Path) -> str:
    return hashlib.sha256(p.read_bytes()).hexdigest()


def run_journey(test_filter: str) -> tuple[int, str]:
    cmd = [
        "cargo",
        "test",
        "-p",
        "dev-e2e",
        "--test",
        "journey",
        test_filter,
        "--",
        "--nocapture",
    ]
    p = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True)
    return p.returncode, p.stdout + p.stderr


def run_case(name: str, rel: str, anchor: str, repl: str, flt: str, stage) -> bool:
    stages = (stage,) if isinstance(stage, str) else tuple(stage)
    path = ROOT / rel
    print(f"\n──────── ▶ 破坏点：{name}（{rel}）")
    before = sha(path)
    backup = path.read_bytes()

    src = path.read_text()
    n = src.count(anchor)
    if n != 1:
        print(f"  BROKEN 锚点命中 {n} 次（必须恰好 1 次）—— 破坏脚本自身失效，先修脚本")
        return False
    path.write_text(src.replace(anchor, repl, 1))
    try:
        rc, out = run_journey(flt)
    finally:
        path.write_bytes(backup)

    hit_tags = [f"[{s}]" for s in stages]
    stage_tag = hit_tags[0]
    hit_stage = any(t in out for t in hit_tags)
    restored = sha(path) == before

    if rc != 0 and hit_stage:
        verdict = "OK"
        print(f"  OK     用例按预期 FAIL，阶段名命中：{[t for t in hit_tags if t in out]}")
        for line in out.splitlines():
            if "panicked" in line or stage_tag in line:
                print(f"         {line.strip()}")
                if "panicked" in line:
                    break
    elif rc != 0:
        verdict = "BROKEN"
        print(f"  BROKEN 失败但没带阶段名 {hit_tags} —— 断言定位不合格")
        for line in out.splitlines():
            if "panicked" in line:
                print(f"         {line.strip()}")
    else:
        verdict = "BROKEN"
        print("  BROKEN 破坏后仍然 PASS —— 这条断言没有真的盯着被测行为")

    if not restored:
        print("  BROKEN 源码未还原（sha256 变化）")
        return False
    return verdict == "OK"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--case", type=int, default=0, help="只跑第 N 个破坏点（1 起）")
    args = ap.parse_args()

    picks = CASES if args.case == 0 else [CASES[args.case - 1]]
    ok = 0
    for i, case in enumerate(picks, start=1):
        # 先确认基线是绿的（红的基线上做破坏没有意义）
        rc, _ = run_journey(case[4])
        if rc != 0:
            print(f"\n基线不绿（{case[4]}），先修用例再做反向验证")
            return 1
        if run_case(*case):
            ok += 1

    total = len(picks)
    print("\n══════════ 反向验证汇总（破坏 → 必须红 → 还原） ══════════")
    print(f"  破坏点 {ok}/{total} 被对应用例按阶段名抓住")
    if ok != total:
        print("  结论: FAIL —— 有破坏点没被抓住（断言不可信）")
        return 1
    print("  结论: PASS —— 全部破坏点都被按阶段名抓住，源码已还原")
    return 0


if __name__ == "__main__":
    sys.exit(main())
