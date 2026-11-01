#!/bin/bash
# 合流验收（卡 t_d99ca3fe，一条命令，退出码 0 = 全过）：
#   bash scripts/merge_acceptance.sh
#
# 为什么需要它：逐卡验收脚本（c6/c7/c8/c9/webcap/ocr_match）的 A 段门槛是「本卡改动白名单」
# （git diff --name-only <本卡 base>...HEAD 只许出现本卡文件）。合流线上一次改动了全部 8 卡的文件，
# 逐卡白名单天然不成立，所以合流卡用这一支做**跨卡不变量**的机器验证：
#
#   A) 迁移编号纪律：注册表严格递增无重号、磁盘 SQL 与注册表一一对应（0014/0015 顺延后仍成立）
#   B) 全量迁移序列可从零应用：0001..0015 顺序灌进空库不报错，FTS5（0014）与组级两列一索引（0015）对象都在
#   C) C9 升级路径（重编号后真跑）：0014 形态旧库（含历史判定行）→ 应用 0015 → 只加两列一索引、
#      历史行数不减、账本追加 15:group_judgement
#   D) 8 条分支的 head 全部是 HEAD 的祖先（合流完整性：一条不落）
#   E) 各卡签名测试仍在（合流没有把别人的用例删掉）：按名字核对
#
# 产出：reports/logs/merge-verify-<ts>/（原始输出）
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
TS="$(date +%Y%m%d-%H%M%S)"
EV="$ROOT/reports/logs/merge-verify-$TS"
mkdir -p "$EV"
FAIL=0
ok()   { echo "PASS  $*"; }
bad()  { echo "FAIL  $*"; FAIL=$((FAIL+1)); }
sec()  { echo; echo "########## $* ##########"; }

sec "A) 迁移编号纪律（注册表 vs 磁盘）"
python3 - "$ROOT" <<'PY' | tee "$EV/a-migrations.txt"
import pathlib, re, sys
root = pathlib.Path(sys.argv[1])
src = (root / "crates/core-storage/src/migrations.rs").read_text(encoding="utf-8")
body = src.split("const MIGRATIONS")[1].split("];")[0]
entries = [(int(v), n, f) for v, n, f in
           re.findall(r'\(\s*(\d+),\s*"([^"]+)",\s*include_str!\("migrations/([^"]+)"\)', body)]
vers = [v for v, _, _ in entries]
disk = sorted(p.name for p in (root / "crates/core-storage/src/migrations").glob("*.sql"))
reg = sorted(f for _, _, f in entries)
print(f"注册 {len(entries)} 条，磁盘 {len(disk)} 个 SQL")
for v, n, f in entries:
    print(f"  {v:>3}  {n:<22} {f}  exists={(root / 'crates/core-storage/src/migrations' / f).is_file()}")
checks = {
    "版本严格递增且无重号": vers == sorted(set(vers)),
    "磁盘 SQL 与注册表一一对应": disk == reg,
    "旧编号残留（0013_ocr_fts / 0014_group_judgement）为零":
        not [p for p in disk if p in ("0013_ocr_fts.sql", "0014_group_judgement.sql")],
    "最高版本 = 15": max(vers) == 15,
}
for k, v in checks.items():
    print(("PASS  " if v else "FAIL  ") + k)
sys.exit(0 if all(checks.values()) else 1)
PY
[ "${PIPESTATUS[0]}" = 0 ] && ok "A 迁移编号纪律" || bad "A 迁移编号纪律"

sec "B) 全量迁移序列 0001..0015 从零应用"
python3 - "$ROOT" <<'PY' | tee "$EV/b-fresh-migrate.txt"
import pathlib, sqlite3, sys, tempfile, os
root = pathlib.Path(sys.argv[1])
files = sorted(p for p in (root / "crates/core-storage/src/migrations").glob("*.sql"))
c = sqlite3.connect(os.path.join(tempfile.mkdtemp(), "fresh.db"))
c.execute("PRAGMA foreign_keys=on")
for f in files:
    try:
        c.executescript(f.read_text(encoding="utf-8"))
    except sqlite3.Error as e:
        print(f"FAIL  应用 {f.name} 报错: {e}")
        sys.exit(1)
objs = {r[0] for r in c.execute("SELECT name FROM sqlite_master")}
rv_cols = [r[1] for r in c.execute("PRAGMA table_info(rule_verdict)")]
checks = {
    "0001..0015 顺序应用无报错": True,
    "0014 的 FTS5 倒排 + 脏队列 + 账本表在盘":
        {"ocr_fts", "ocr_fts_dirty", "ocr_fts_state"} <= objs,
    "0014 的 4 个同步触发器在盘":
        {"ocr_fts_ai", "ocr_fts_au", "ocr_fts_aud", "ocr_fts_ad"} <= objs,
    "0015 的组级两列在 rule_verdict": {"judgementScope", "groupKey"} <= set(rv_cols),
    "0015 的组键索引在盘": "idx_rule_verdict_group" in objs,
}
for k, v in checks.items():
    print(("PASS  " if v else "FAIL  ") + k)
print("应用文件:", ", ".join(f.name for f in files))
sys.exit(0 if all(checks.values()) else 1)
PY
[ "${PIPESTATUS[0]}" = 0 ] && ok "B 全量迁移从零应用" || bad "B 全量迁移从零应用"

sec "C) C9 升级路径（0014 形态旧库 → 0015，重编号后真跑）"
python3 - "$ROOT" <<'PY' | tee "$EV/c-c9-upgrade.txt"
import pathlib, sqlite3, sys, tempfile, os
root = pathlib.Path(sys.argv[1])
mig = root / "crates/core-storage/src/migrations"
old = sorted(p for p in mig.glob("*.sql") if p.name < "0015")
new = mig / "0015_group_judgement.sql"
db = os.path.join(tempfile.mkdtemp(), "old.db")
c = sqlite3.connect(db)
c.execute("PRAGMA foreign_keys=on")
for f in old:
    c.executescript(f.read_text(encoding="utf-8"))
c.execute("CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, applied_at INTEGER NOT NULL)")
for i, f in enumerate(old, start=1):
    c.execute("INSERT INTO schema_migrations VALUES (?,?,0)", (i, f.stem))
c.execute("""INSERT INTO assets (assetId, absPath, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (101, '/corpus/k01.jpg', 'k01.jpg', 'jpg', 1, 1, 1)""")
c.execute("""INSERT INTO rule_verdict (packId, assetId, ruleId, result, score, ts)
             VALUES ('ecommerce-taobao', 101, 'R1_size', 'pass', 0.9, 1)""")
c.commit()
before_rows = c.execute("SELECT COUNT(*) FROM rule_verdict").fetchone()[0]
before_cols = [r[1] for r in c.execute("PRAGMA table_info(rule_verdict)")]
c.executescript(new.read_text(encoding="utf-8"))
c.execute("INSERT INTO schema_migrations VALUES (15,'group_judgement',0)")
c.commit()
after_rows = c.execute("SELECT COUNT(*) FROM rule_verdict").fetchone()[0]
after_cols = [r[1] for r in c.execute("PRAGMA table_info(rule_verdict)")]
idx = {r[0] for r in c.execute("SELECT name FROM sqlite_master WHERE type='index'")}
ledger = [f"{v}:{n}" for v, n in c.execute("SELECT version, name FROM schema_migrations ORDER BY version")]
checks = {
    "旧库形态（无组级两列）": not {"judgementScope", "groupKey"} & set(before_cols),
    "升级只加两列": set(after_cols) - set(before_cols) == {"judgementScope", "groupKey"},
    "既有列一个不少": set(before_cols) <= set(after_cols),
    "组键索引新建": "idx_rule_verdict_group" in idx,
    "历史判定行不减": after_rows >= before_rows,
    "账本追加 15:group_judgement": ledger[-1] == "15:group_judgement",
}
for k, v in checks.items():
    print(("PASS  " if v else "FAIL  ") + k)
print(f"行数 {before_rows} -> {after_rows}；账本尾: {ledger[-1]}")
sys.exit(0 if all(checks.values()) else 1)
PY
[ "${PIPESTATUS[0]}" = 0 ] && ok "C C9 升级路径" || bad "C C9 升级路径"

sec "C2) 0011 旧库 → 最新（数据不丢 + 各卡新列在位；卡验收 3）"
python3 - "$ROOT" <<'PYPATCH' | tee "$EV/c2-0011-to-head.txt"
import pathlib, sqlite3, sys, tempfile, os
root = pathlib.Path(sys.argv[1])
mig = root / "crates/core-storage/src/migrations"
old = sorted(p for p in mig.glob("*.sql") if p.name <= "0011_rule_verdict_feedback.sql")
new = sorted(p for p in mig.glob("*.sql") if p.name > "0011_rule_verdict_feedback.sql")
db = os.path.join(tempfile.mkdtemp(), "v11.db")
c = sqlite3.connect(db)
c.execute("PRAGMA foreign_keys=on")
for f in old:
    c.executescript(f.read_text(encoding="utf-8"))
# 0011 形态的历史数据（升级前就存在的业务行，一字不能丢）
c.execute("""INSERT INTO assets (assetId, absPath, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (1,'/lib/a.jpg','a.jpg','jpg',10,1,1),(2,'/lib/b.jpg','b.jpg','jpg',10,2,2)""")
c.execute("""INSERT INTO tag (assetId, name, origin, createdAt) VALUES (1,'历史标签','manual',1)""")
c.execute("""INSERT INTO rule_verdict (packId, assetId, ruleId, result, score, ts)
             VALUES ('ecommerce-taobao',1,'R1_size','pass',0.9,1),
                    ('ecommerce-taobao',2,'R1_size','fail',0.2,2)""")
c.commit()
before = {t: c.execute("SELECT COUNT(*) FROM " + t).fetchone()[0]
          for t in ("assets", "tag", "rule_verdict")}
for f in new:
    c.executescript(f.read_text(encoding="utf-8"))
c.commit()
after = {t: c.execute("SELECT COUNT(*) FROM " + t).fetchone()[0] for t in before}
a_cols = {r[1] for r in c.execute("PRAGMA table_info(assets)")}
v_cols = {r[1] for r in c.execute("PRAGMA table_info(rule_verdict)")}
objs = {r[0] for r in c.execute("SELECT name FROM sqlite_master")}
checks = {
    "0011 → 最新共升 %d 步无报错" % len(new): True,
    "历史行一条不丢（assets/tag/rule_verdict）": all(after[t] == before[t] for t in before),
    "OCR（0012）assets.ocr_text 在位": "ocr_text" in a_cols,
    "EXIF（0013）assets 元数据列在位": {"takenAt", "cameraMake", "cameraModel", "lensModel", "iso"} <= a_cols,
    "OCR-MATCH（0014）FTS5 倒排对象在位": {"ocr_fts", "ocr_fts_dirty", "ocr_fts_state"} <= objs,
    "C9（0015）rule_verdict 组级两列 + 索引在位":
        {"judgementScope", "groupKey"} <= v_cols and "idx_rule_verdict_group" in objs,
}
for k, v in checks.items():
    print(("PASS  " if v else "FAIL  ") + k)
print("升级前:", before, "升级后:", after, "升级步:", ", ".join(f.name for f in new))
sys.exit(0 if all(checks.values()) else 1)
PYPATCH
[ "${PIPESTATUS[0]}" = 0 ] && ok "C2 0011 → 最新升级" || bad "C2 0011 → 最新升级"

sec "D) 8 条分支合流完整性（head 均为 HEAD 祖先）"
for b in t_df883cdd t_e8c656a4 t_9fe8501e t_17ee7d28 t_ff666f9c t_43212e61 t_1a576f41 t_d8b84809; do
  if git merge-base --is-ancestor "gitea/wt/$b" HEAD 2>/dev/null; then
    ok "wt/$b $(git rev-parse --short "gitea/wt/$b") 已并入"
  else
    bad "wt/$b 未并入"
  fi
done | tee "$EV/d-ancestors.txt"

sec "E) 各卡签名测试仍在（按名字核对）"
python3 - "$ROOT" <<'PY' | tee "$EV/e-signature-tests.txt"
import pathlib, re, sys
root = pathlib.Path(sys.argv[1])
tests = []
for p in list((root / "crates").rglob("*.rs")) + list((root / "web/tests").rglob("*.ts*")):
    if "/target/" in str(p):
        continue
    try:
        t = p.read_text(encoding="utf-8", errors="ignore")
    except OSError:
        continue
    tests += re.findall(r"fn ([a-z0-9_]*(?:literal_fusion|ocr_fuse|ocr_index|ocr_snippet|group_judge|capabilit|c9_|c8_|judge)[a-z0-9_]*)", t)
    tests += re.findall(r"(?:it|test)\(\s*['\"]([^'\"]*文字命中[^'\"]*|[^'\"]*capabilit[^'\"]*)", t)
tests = sorted(set(tests))
need = {"literal_fusion_recalls_by_filename_title_tag_and_ocr",
        "ocr_fuse_raises_semantic_and_adds_literal_only_hit"}
missing = {n for n in need if n not in tests}
print(f"命中签名测试 {len(tests)} 个（示例 {tests[:8]}）")
print(("PASS  " if not missing else "FAIL  ") + f"D3/OCR-MATCH 关键用例在盘（缺 {missing or '无'}）")
sys.exit(0 if not missing else 1)
PY
[ "${PIPESTATUS[0]}" = 0 ] && ok "E 签名测试" || bad "E 签名测试"

echo
[ "$FAIL" -eq 0 ] && echo "MERGE_ACCEPTANCE_RESULT: ALL PASS（证据 $EV）" || echo "MERGE_ACCEPTANCE_RESULT: $FAIL 段失败（证据 $EV）"
[ "$FAIL" -eq 0 ] || exit 1
