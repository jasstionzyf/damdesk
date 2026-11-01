#!/usr/bin/env python3
"""DECouple-BE（卡 `t_59942f03`）：`verdict_human_review` → `human_review` 的**生产纪律**迁移脚本。

为什么单独一个脚本（启动迁移只做加法）：
* 迁移 `0035_human_review_v2` 只负责**建新表 + 幂等搬行**（`INSERT OR IGNORE` + `legacyId` 唯一索引）；
  旧表的 **purge 必须有人验过才做** —— 在启动路径里 `DROP TABLE` 一旦搬漏就没有回头路。
* 本脚本按「全量备份 + sha256 → 幂等搬 → 回读校验 100% → 切读写（代码已切）→ purge 旧表」执行，
  每一步都可复跑、都留证据（stdout 的 `[step]` 行可直接贴卡）。

用法::

    # 只看会做什么（不写库、不备份、不 purge）
    python3 scripts/migrate_human_review_v2.py --db /path/to/dam.db --dry-run

    # 实跑：备份 → 搬（若 0035 未应用则就地应用）→ 回读校验 → purge（默认不 purge，需显式 --purge）
    python3 scripts/migrate_human_review_v2.py --db /path/to/dam.db
    python3 scripts/migrate_human_review_v2.py --db /path/to/dam.db --purge

    # 自检（造 0034 形态旧库 + 两条人审 → 全流程走一遍，退出码 0 = 全过）
    python3 scripts/migrate_human_review_v2.py --self-test

退出码：0 全过 / 1 校验不符或执行错误 / 2 参数错误。
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import sqlite3
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
MIGR_DIR = REPO / "crates/core-storage/src/migrations"
NEW_MIGRATION = MIGR_DIR / "0035_human_review_v2.sql"
NEW_VERSION = 35
NEW_NAME = "human_review_v2"

# 逐行回读校验：旧表每一行都必须在新表里有**字段级等价**的一行（legacyId 对齐）
VERIFY_SQL = """
SELECT COUNT(*) FROM verdict_human_review v
LEFT JOIN human_review h
       ON h.legacyId = v.id
      AND h.assetId  = v.assetId
      AND h.schemeId = 'pack:' || v.packId
      AND h.ruleId IS v.ruleId
      AND h.humanStatus  = v.humanStatus
      AND h.humanReason  = v.humanReason
      AND h.judgedAt     = v.humanAt
      AND COALESCE(h.revokedAt, 0) = COALESCE(v.revokedAt, 0)
WHERE h.id IS NULL
"""


def say(step: str, msg: str) -> None:
    print(f"[{step}] {msg}", flush=True)


def table_exists(conn: sqlite3.Connection, name: str) -> bool:
    row = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", (name,)
    ).fetchone()
    return row is not None


def count(conn: sqlite3.Connection, sql: str) -> int:
    return int(conn.execute(sql).fetchone()[0])


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def backup(db: Path, backup_dir: Path | None) -> Path | None:
    """在线备份（sqlite backup API，不怕 WAL 未落盘）+ 备份文件 sha256。"""
    dest_dir = backup_dir or db.parent
    dest_dir.mkdir(parents=True, exist_ok=True)
    base = f"{db.name}.pre-human_review_v2.{time.strftime('%Y%m%d-%H%M%S')}"
    dest = dest_dir / f"{base}.bak"
    n = 1
    while dest.exists():  # 同一秒内重复跑也要各留一份（绝不覆盖备份）
        dest = dest_dir / f"{base}-{n}.bak"
        n += 1
    src = sqlite3.connect(str(db))
    try:
        dst = sqlite3.connect(str(dest))
        try:
            src.backup(dst)
        finally:
            dst.close()
    finally:
        src.close()
    say("backup", f"{dest} sha256={sha256(dest)}")
    return dest


def migration_applied(conn: sqlite3.Connection) -> bool:
    if table_exists(conn, "human_review"):
        return True
    if table_exists(conn, "schema_migrations"):
        row = conn.execute(
            "SELECT 1 FROM schema_migrations WHERE name = ?", (NEW_NAME,)
        ).fetchone()
        if row:
            return True
    return False


def apply_migration(conn: sqlite3.Connection, dry_run: bool) -> None:
    """应用 0035（幂等）：建表 + 搬行 + 记账本（与 Rust `migrations.rs` 同一 (version, name)）。"""
    if migration_applied(conn):
        say("migrate", "0035 已应用（human_review 存在或账本已记）→ 跳过")
        return
    if dry_run:
        say("migrate", f"[dry-run] 将应用 {NEW_MIGRATION.name}（版本 {NEW_VERSION}）")
        return
    sql = NEW_MIGRATION.read_text(encoding="utf-8")
    conn.executescript(sql)
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations (\n"
        "    version INTEGER PRIMARY KEY, name TEXT NOT NULL, applied_at INTEGER NOT NULL)"
    )
    conn.execute(
        "INSERT OR IGNORE INTO schema_migrations (version, name, applied_at) VALUES (?,?,?)",
        (NEW_VERSION, NEW_NAME, int(time.time())),
    )
    conn.commit()
    say("migrate", f"已应用 {NEW_MIGRATION.name}")


def snapshot(conn: sqlite3.Connection) -> dict:
    out = {
        "db_version": None,
        "old_total": None,
        "old_active": None,
        "new_total": count(conn, "SELECT COUNT(*) FROM human_review") if table_exists(conn, "human_review") else None,
        "new_active": None,
        "new_from_legacy": None,
    }
    if table_exists(conn, "schema_migrations"):
        row = conn.execute("SELECT MAX(version) FROM schema_migrations").fetchone()
        out["db_version"] = row[0]
    if table_exists(conn, "verdict_human_review"):
        out["old_total"] = count(conn, "SELECT COUNT(*) FROM verdict_human_review")
        out["old_active"] = count(
            conn, "SELECT COUNT(*) FROM verdict_human_review WHERE revokedAt IS NULL"
        )
    if table_exists(conn, "human_review"):
        out["new_active"] = count(
            conn, "SELECT COUNT(*) FROM human_review WHERE revokedAt IS NULL"
        )
        out["new_from_legacy"] = count(
            conn, "SELECT COUNT(*) FROM human_review WHERE legacyId IS NOT NULL"
        )
    return out


def verify(conn: sqlite3.Connection) -> tuple[int, int, list[str]]:
    """返回 (旧表行数, 未对上行数, 抽样问题描述)。旧表不存在 → (0, 0, [])。"""
    if not table_exists(conn, "verdict_human_review"):
        return 0, 0, []
    if not table_exists(conn, "human_review"):
        return count(conn, "SELECT COUNT(*) FROM verdict_human_review"), -1, ["human_review 表不存在"]
    old = count(conn, "SELECT COUNT(*) FROM verdict_human_review")
    bad = count(conn, VERIFY_SQL)
    samples: list[str] = []
    if bad:
        cur = conn.execute(
            "SELECT v.id, v.packId, v.assetId, v.ruleId, v.humanStatus FROM verdict_human_review v "
            "LEFT JOIN human_review h ON h.legacyId = v.id WHERE h.id IS NULL LIMIT 5"
        )
        samples = [f"legacyId={r[0]} pack={r[1]} asset={r[2]} rule={r[3]} status={r[4]}" for r in cur]
    return old, bad, samples


def purge(conn: sqlite3.Connection, dry_run: bool) -> None:
    if not table_exists(conn, "verdict_human_review"):
        say("purge", "旧表已不存在（已是 purge 后形态）→ 跳过")
        return
    if dry_run:
        say("purge", "[dry-run] 将 DROP TABLE verdict_human_review")
        return
    conn.execute("DROP TABLE verdict_human_review")
    conn.commit()
    say("purge", "已 DROP TABLE verdict_human_review")


def run(db: Path, dry_run: bool, do_purge: bool, backup_dir: Path | None) -> int:
    if not db.exists():
        say("error", f"库不存在: {db}")
        return 2
    conn = sqlite3.connect(str(db))
    conn.isolation_level = None  # 自己管事务（executescript 需要）
    try:
        say("inspect", f"db={db} size={db.stat().st_size} sha256={sha256(db)}")
        pre = snapshot(conn)
        say("inspect", f"pre  {json.dumps(pre, ensure_ascii=False)}")
        apply_migration(conn, dry_run)
        post_apply = snapshot(conn)
        say("inspect", f"post-apply {json.dumps(post_apply, ensure_ascii=False)}")

        if dry_run and not table_exists(conn, "human_review"):
            old_total = (
                count(conn, "SELECT COUNT(*) FROM verdict_human_review")
                if table_exists(conn, "verdict_human_review")
                else 0
            )
            say("verify", f"[dry-run] 将迁移 {old_total} 行并逐行回读校验（本次不写库）")
            say("done", "dry-run 结束（未改动任何数据）")
            return 0
        old, bad, samples = verify(conn)
        if bad < 0:
            say("verify", f"FAIL: {samples}")
            return 1
        rate = 100.0 if old == 0 else (old - bad) * 100.0 / old
        say("verify", f"旧表 {old} 行 / 未对上 {bad} 行 / 回读一致率 {rate:.2f}%")
        if bad:
            for s in samples:
                say("verify", f"  · {s}")
            say("verify", "FAIL：搬漏行，禁止 purge")
            return 1
        if not dry_run:
            backup_used = backup(db, backup_dir)
            say("backup", f"done: {backup_used}")
        if do_purge:
            purge(conn, dry_run)
        else:
            say("purge", "未要求 purge（加 --purge 才删旧表）")
        post = snapshot(conn)
        say("inspect", f"post {json.dumps(post, ensure_ascii=False)}")
        # 幂等复核：再跑一遍「应用 + 校验」，行数必须一模一样
        apply_migration(conn, dry_run)
        again = snapshot(conn)
        if again["new_total"] != post["new_total"]:
            say("idempotent", f"FAIL：重复执行后行数变化 {post['new_total']} → {again['new_total']}")
            return 1
        say("idempotent", f"OK：重复执行后 human_review 行数仍为 {again['new_total']}")
        say("done", "迁移完成")
        return 0
    finally:
        conn.close()


# ---------------------------------------------------------------- 自检（造 0034 形态旧库）


def build_legacy_db(path: Path) -> None:
    """按 0001..0034 的 SQL 造一个「0035 之前」的库并灌一条旧人审 + 一条已撤销人审。"""
    conn = sqlite3.connect(str(path))
    conn.isolation_level = None
    files = sorted(MIGR_DIR.glob("00[0-3][0-9]_*.sql"))
    files = [f for f in files if int(f.name[:4]) <= 34]
    for f in files:
        conn.executescript(f.read_text(encoding="utf-8"))
    ts = int(time.time())
    conn.execute(
        "INSERT INTO assets (assetId, absPath, mediaType, fileName, fileExt, fileSz, mtime, fileHash, importedAt) "
        "VALUES (1, '/x/a.jpg', 'image', 'a.jpg', 'jpg', 10, 0, 'hash-a', ?)",
        (ts,),
    )
    conn.execute(
        "INSERT INTO assets (assetId, absPath, mediaType, fileName, fileExt, fileSz, mtime, fileHash, importedAt) "
        "VALUES (2, '/x/b.jpg', 'image', 'b.jpg', 'jpg', 10, 0, 'hash-b', ?)",
        (ts,),
    )
    conn.execute(
        "INSERT INTO verdict_human_review (packId, assetId, ruleId, judgementScope, groupKey, humanStatus, "
        "humanReason, humanAt, reviewedBy, machineResult, machineConfidence) "
        "VALUES ('com.soujpg.pack.ecommerce', 1, 'R1_size', 'per_asset', '', 'pass', '阈值过严', ?, 'tester', 'fail', 0.3)",
        (ts,),
    )
    conn.execute(
        "INSERT INTO verdict_human_review (packId, assetId, ruleId, judgementScope, groupKey, humanStatus, "
        "humanReason, humanAt, reviewedBy, machineResult, machineConfidence) "
        "VALUES ('com.soujpg.pack.ecommerce', 2, NULL, 'per_asset', '', 'fail', '整图模糊', ?, 'tester', 'undetermined', 0.9)",
        (ts,),
    )
    conn.execute(
        "INSERT INTO verdict_human_review (packId, assetId, ruleId, judgementScope, groupKey, humanStatus, "
        "humanReason, humanAt, reviewedBy, machineResult, machineConfidence, revokedAt, revokeReason) "
        "VALUES ('com.soujpg.pack.ecommerce', 2, 'R2_white_bg', 'per_asset', '', 'pass', '撤销的旧裁决', ?, NULL, 'fail', 0.4, ?, 'superseded')",
        (ts, ts),
    )
    conn.commit()
    conn.close()


def self_test() -> int:
    tmp = Path(tempfile.mkdtemp(prefix="human-review-v2-selftest-"))
    db = tmp / "dam.db"
    build_legacy_db(db)
    say("selftest", f"夹具库 {db}")
    # 1) dry-run 不写库
    before = sha256(db)
    rc = run(db, dry_run=True, do_purge=False, backup_dir=tmp / "bak")
    if rc != 0:
        say("selftest", f"FAIL dry-run rc={rc}")
        return 1
    if sha256(db) != before:
        say("selftest", "FAIL dry-run 改动了库")
        return 1
    # 2) 实跑 + purge
    rc = run(db, dry_run=False, do_purge=True, backup_dir=tmp / "bak")
    if rc != 0:
        say("selftest", f"FAIL real rc={rc}")
        return 1
    backups = list((tmp / "bak").glob("*.bak"))
    if len(backups) != 1:
        say("selftest", f"FAIL 备份数量 {len(backups)}")
        return 1
    # 3) purge 后再跑（幂等，不许报错）
    rc = run(db, dry_run=False, do_purge=True, backup_dir=tmp / "bak")
    if rc != 0:
        say("selftest", f"FAIL rerun rc={rc}")
        return 1
    conn = sqlite3.connect(str(db))
    try:
        if table_exists(conn, "verdict_human_review"):
            say("selftest", "FAIL 旧表仍在")
            return 1
        n = count(conn, "SELECT COUNT(*) FROM human_review")
        active = count(conn, "SELECT COUNT(*) FROM human_review WHERE revokedAt IS NULL")
        schemes = count(
            conn, "SELECT COUNT(*) FROM human_review WHERE schemeId = 'pack:com.soujpg.pack.ecommerce'"
        )
        if (n, active, schemes) != (3, 2, 3):
            say("selftest", f"FAIL 行数 (total,active,scheme)=( {n},{active},{schemes}) != (3,2,3)")
            return 1
        ver = conn.execute("SELECT MAX(version) FROM schema_migrations").fetchone()[0]
        if ver != NEW_VERSION:
            say("selftest", f"FAIL 账本版本 {ver} != {NEW_VERSION}")
            return 1
    finally:
        conn.close()
    say("selftest", "PASS：dry-run 不改库 / 搬家 3 行（2 活 1 撤销）/ purge 后幂等 / 备份 1 份")
    shutil.rmtree(tmp, ignore_errors=True)
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description="verdict_human_review → human_review 迁移")
    ap.add_argument("--db", help="dam.db 路径")
    ap.add_argument("--dry-run", action="store_true", help="只看计划，不写库不备份")
    ap.add_argument("--purge", action="store_true", help="回读校验 100%% 后 DROP 旧表")
    ap.add_argument("--backup-dir", help="备份目录（缺省与库同目录）")
    ap.add_argument("--self-test", action="store_true", help="造旧库跑全流程自检")
    ap.add_argument(
        "--seed-legacy",
        metavar="DB",
        help="（QA 用）按 0001..0034 造一个「升级前」的库并灌 3 条旧人审，供 dry-run/实跑演练",
    )
    a = ap.parse_args()
    if a.self_test:
        return self_test()
    if a.seed_legacy:
        path = Path(a.seed_legacy)
        if path.exists():
            say("error", f"{path} 已存在，拒绝覆盖（换个路径或先删）")
            return 2
        path.parent.mkdir(parents=True, exist_ok=True)
        build_legacy_db(path)
        conn = sqlite3.connect(str(path))
        try:
            n = count(conn, "SELECT COUNT(*) FROM verdict_human_review")
            ver = conn.execute(
                "SELECT COALESCE(MAX(version),0) FROM schema_migrations"
            ).fetchone()[0] if table_exists(conn, "schema_migrations") else 0
        finally:
            conn.close()
        say("seed", f"{path} 已建：schema_version={ver}（0035 未应用） 旧人审 {n} 行")
        return 0
    if not a.db:
        ap.error("--db 必填（或用 --self-test）")
    return run(Path(a.db), a.dry_run, a.purge, Path(a.backup_dir) if a.backup_dir else None)


if __name__ == "__main__":
    sys.exit(main())
