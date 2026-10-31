#!/usr/bin/env python3
"""标签总览（task）规模实测：造 7.5M 行 tag / 10^4 值 的合成库并计时关键查询。

用途：`/tags` 页面「左栏 key 分组（值数/资产数）」是唯一需要全库聚合的读路径，
必须先用实测决定用哪种聚合口径（带 assets 连接 vs 纯 tag 聚合），再落 Rust。
验收口径（卡 task §验收 1）：`/tags` 打开 < 1.5s（10^4 值规模）。

用法：
  python3 scripts/bench_tag_scale.py --dir /data5/dam-bench-tags          # 造库 + 计时
  python3 scripts/bench_tag_scale.py --dir /data5/dam-bench-tags --bench  # 只计时（库已存在）

幂等：库文件已存在且行数达标则跳过造数（--force 重建）。
"""
from __future__ import annotations

import argparse
import os
import random
import sqlite3
import time

ASSETS = 1_000_000
TAGS = 7_500_000
KEYED_VALUES = 6_000  # `key{n}-val{n}` 形态
BARE_VALUES = 4_000  # 无 `-` 的裸值
KEYS = 200

DDL = """
PRAGMA journal_mode=WAL;
PRAGMA synchronous=OFF;
CREATE TABLE assets (
    assetId INTEGER PRIMARY KEY,
    mediaType TEXT NOT NULL DEFAULT 'image',
    absPath TEXT NOT NULL,
    missing INTEGER NOT NULL DEFAULT 0,
    fileName TEXT NOT NULL,
    fileExt TEXT NOT NULL,
    fileSz INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    deleted INTEGER NOT NULL DEFAULT 0,
    importedAt INTEGER NOT NULL
);
CREATE INDEX idx_assets_deleted ON assets(deleted);
CREATE TABLE tag (
    id INTEGER PRIMARY KEY,
    assetId INTEGER NOT NULL,
    value TEXT NOT NULL,
    origin TEXT NOT NULL DEFAULT 'auto',
    model TEXT,
    taskId INTEGER,
    score REAL NOT NULL DEFAULT 1.0,
    createdAt INTEGER NOT NULL
);
CREATE UNIQUE INDEX uq_tag_asset_value ON tag(assetId, value);
CREATE INDEX idx_tag_value ON tag(value);
CREATE INDEX idx_tag_value_nocase ON tag(value COLLATE NOCASE);
"""


def values() -> list[str]:
    out = [f"key{i % KEYS}-val{i}" for i in range(KEYED_VALUES)]
    out += [f"bare{i}" for i in range(BARE_VALUES)]
    return out


def build(db: str, force: bool) -> None:
    if os.path.exists(db) and not force:
        c = sqlite3.connect(db)
        try:
            n = c.execute("SELECT COUNT(*) FROM tag").fetchone()[0]
        except sqlite3.Error:
            n = 0
        c.close()
        if n >= TAGS:
            print(f"[build] 已有 {n} 行 tag，跳过造数")
            return
    if os.path.exists(db):
        os.remove(db)
    c = sqlite3.connect(db)
    c.executescript(DDL)
    print("[build] assets ...")
    c.executemany(
        "INSERT INTO assets (assetId, absPath, fileName, fileExt, fileSz, mtime, importedAt, deleted) "
        "VALUES (?,?,?,?,?,?,?,?)",
        (
            (i, f"/lib/img{i:08d}.jpg", f"img{i:08d}.jpg", "jpg", 1024, 0, 0, 1 if i % 1000 == 0 else 0)
            for i in range(1, ASSETS + 1)
        ),
    )
    c.commit()
    print("[build] tags ...")
    vals = values()
    rnd = random.Random(20261020)
    # 每张资产 ~7.5 个标签；值分布带热度倾斜（前 5% 值占 40% 行）
    hot = vals[: len(vals) // 20]
    cold = vals[len(vals) // 20 :]

    def gen():
        tid = 0
        for a in range(1, ASSETS + 1):
            n = 5 + (a % 6)  # 5..10 个标签/资产
            picked: set[str] = set()
            while len(picked) < n:
                if rnd.random() < 0.4:
                    picked.add(hot[rnd.randrange(len(hot))])
                else:
                    picked.add(cold[rnd.randrange(len(cold))])
            for v in picked:
                tid += 1
                yield (tid, a, v, "auto", 1.0, 1)

    cur = c.cursor()
    batch: list[tuple] = []
    for row in gen():
        batch.append(row)
        if len(batch) >= 50_000:
            cur.executemany(
                "INSERT INTO tag (id, assetId, value, origin, score, createdAt) VALUES (?,?,?,?,?,?)", batch
            )
            batch.clear()
    if batch:
        cur.executemany(
            "INSERT INTO tag (id, assetId, value, origin, score, createdAt) VALUES (?,?,?,?,?,?)", batch
        )
    c.commit()
    c.execute("ANALYZE")
    c.commit()
    n = c.execute("SELECT COUNT(*) FROM tag").fetchone()[0]
    v = c.execute("SELECT COUNT(DISTINCT value) FROM tag").fetchone()[0]
    print(f"[build] tag={n} distinct_value={v} size={os.path.getsize(db)/1e6:.0f}MB")
    c.close()


# 候选实现（与 Rust 侧待落方案一一对应）
Q_GROUPS_FULL = """
SELECT CASE WHEN instr(value,'-')>1 THEN substr(value,1,instr(value,'-')-1) ELSE '' END AS k,
       COUNT(DISTINCT t.value) AS vals, COUNT(DISTINCT t.assetId) AS assets
FROM tag t CROSS JOIN assets a ON a.assetId = t.assetId AND a.deleted = 0
GROUP BY k ORDER BY assets DESC, k
"""
Q_GROUPS_TAG_ONLY = """
SELECT CASE WHEN instr(value,'-')>1 THEN substr(value,1,instr(value,'-')-1) ELSE '' END AS k,
       COUNT(DISTINCT t.value) AS vals, COUNT(t.assetId) AS assets
FROM tag t
GROUP BY k ORDER BY assets DESC, k
"""
Q_VALUE_DICT = "SELECT value FROM tag INDEXED BY idx_tag_value_nocase GROUP BY value"
Q_KEY_PAGE = """
SELECT t.value, COUNT(DISTINCT t.assetId) AS n
FROM tag t CROSS JOIN assets a ON a.assetId = t.assetId AND a.deleted = 0
WHERE t.value >= ?1 COLLATE NOCASE AND t.value < ?2 COLLATE NOCASE
GROUP BY t.value ORDER BY n DESC, t.value LIMIT 50 OFFSET 0
"""
Q_KEY_TOTAL = """
SELECT COUNT(DISTINCT t.value) FROM tag t
CROSS JOIN assets a ON a.assetId = t.assetId AND a.deleted = 0
WHERE t.value >= ?1 COLLATE NOCASE AND t.value < ?2 COLLATE NOCASE
"""
Q_ONE_VALUE = """
SELECT COUNT(DISTINCT t.assetId) FROM tag t
CROSS JOIN assets a ON a.assetId = t.assetId AND a.deleted = 0
WHERE t.value = ?1 COLLATE NOCASE
"""
Q_SAMPLES = """
SELECT t.assetId FROM tag t CROSS JOIN assets a ON a.assetId = t.assetId AND a.deleted = 0
WHERE t.value = ?1 COLLATE NOCASE LIMIT 3
"""


def bench(db: str, rounds: int = 3) -> None:
    c = sqlite3.connect(db)
    print(f"[bench] tag={c.execute('SELECT COUNT(*) FROM tag').fetchone()[0]} "
          f"distinct={c.execute('SELECT COUNT(DISTINCT value) FROM tag').fetchone()[0]}")

    def run(name: str, sql: str, args: tuple = ()) -> None:
        best = None
        rows = 0
        for _ in range(rounds):
            t0 = time.perf_counter()
            got = c.execute(sql, args).fetchall()
            dt = (time.perf_counter() - t0) * 1000
            rows = len(got)
            best = dt if best is None else min(best, dt)
        print(f"  {name:<22} {best:8.1f} ms  rows={rows}")

    run("groups_full", Q_GROUPS_FULL)
    run("groups_tag_only", Q_GROUPS_TAG_ONLY)
    run("value_dict_index", Q_VALUE_DICT)
    run("key_page(50)", Q_KEY_PAGE, ("key3-", "key3-\U0010ffff"))
    run("key_total", Q_KEY_TOTAL, ("key3-", "key3-\U0010ffff"))
    run("single_value_count", Q_ONE_VALUE, ("key3-val3",))
    run("samples(3)", Q_SAMPLES, ("key3-val3",))

    print("\n[explain] groups_full")
    for r in c.execute("EXPLAIN QUERY PLAN " + Q_GROUPS_FULL):
        print("   ", r[-1])
    print("[explain] key_page(50)")
    for r in c.execute("EXPLAIN QUERY PLAN " + Q_KEY_PAGE, ("key3-", "key3-\U0010ffff")):
        print("   ", r[-1])
    c.close()


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dir", default="/data5/dam-bench-tags")
    ap.add_argument("--force", action="store_true", help="重建库")
    ap.add_argument("--bench", action="store_true", help="只计时（跳过造数）")
    a = ap.parse_args()
    os.makedirs(a.dir, exist_ok=True)
    db = os.path.join(a.dir, "library.db")
    if not a.bench:
        build(db, a.force)
    bench(db)


if __name__ == "__main__":
    main()
