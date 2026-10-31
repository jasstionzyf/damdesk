#!/usr/bin/env python3
"""CMP-JOB 跨层 E2E 复验（卡 task）。

为什么要这个脚本（不是再跑一遍 cmp_job02_acceptance.sh）：
  父卡 task / task 各自只验证了自己的那一层 —— 后端有 svc-pipeline 单测
  （内存库 + 直接调函数），前端有 jsdom 单测，界面探针跑的是**他们自己造的三次执行**
  （done / cancelled / interrupted，24 张完好真图 → skipped 恒为 0，failed 8 张）。
  「一次批量审核 = 一个独立 job」最容易错的恰恰是三层之间：
    · 跳过在 job_queue 里与成功同终态（done），只能由 run 行上的计数器区分 →
      HTTP 响应里 skipped 是否真的 != failed、是否计入 total、是否重复计数；
    · 上一次取消/中断留下的 pending 任务会被下一轮**认领**（adopt_pending）→
      认领之后「各次的数字还稳定吗」；
    · 迁移前 runId IS NULL 的历史失败行 → 是否真的不进任何一次；
    · 前端把 runId 口径换成累计口径（或反之）时，界面数字与数据库事实是否还对得上。

本脚本在**一个进程、一个真 SQLite、真 HTTP、真 VL（同网段 vLLM）**上跑 5 次执行，
每次执行后同时读三处事实并互相校验：
    1) 产品 API（/api/compliance/jobs、/jobs/{runId}、/judge/status）
    2) 数据库（job_queue WHERE runId=? 的实际行）
    3) UI（由 cmp_job_e2e_ui.mjs 在 CDP 上读 DOM，与本脚本落盘的 API 快照比对）

用法（在容器内，由 scripts/cmp_job_e2e.sh 调用）：
  python3 scripts/cmp_job_e2e.py --out reports/evidence/CMP-JOB-E2E
退出码：0 全 PASS；非 0 = FAIL 条数。证据 JSON 落在 --out/assertions.json。
"""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import signal
import sqlite3
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(os.environ.get("CMP_ROOT", "/workspace"))
WORK = Path(os.environ.get("CMP_WORK", "/tmp/cmpjobE2E"))
DATA = WORK / "data"
CORPUS = WORK / "corpus"
APP = os.environ.get("CMP_APP_BIN", "/cargo-target/dam/debug/app-web")
DIST = ROOT / "web/dist"
PORT = int(os.environ.get("CMP_PORT", "18899"))
NIMG = int(os.environ.get("CMP_IMAGES", "10"))
VL_BASE = os.environ.get("CMP_VL_BASE_URL", "")
VL_MODEL = os.environ.get("CMP_VL_MODEL", "qwen3.5-4b")
API = f"http://127.0.0.1:{PORT}"

RESULTS: list[dict] = []

# 跳过原因前缀（**与后端 `crates/svc-pipeline/src/skip_reason.rs` 的 `SKIP_PREFIXES` 同一份口径**，
# 也与界面 `web/src/lib/complianceJobs.ts` 的 `SKIP_PREFIXES` 同口径）。
# 不变量：run.skipped == 明细里带这些前缀的 task 数（P5 硬断言）。
SKIP_PREFIXES = ("deleted:", "file_missing:", "not_image_media:", "not_image:")


def is_skip_reason(last_error: str | None) -> bool:
    s = (last_error or "").strip()
    return any(s.startswith(p) for p in SKIP_PREFIXES)


# ------------------------------------------------------------------ 断言记账
def ok(name: str, detail="") -> None:
    RESULTS.append({"name": name, "ok": True, "detail": detail})
    print(f"PASS {name} :: {detail}", flush=True)


def bad(name: str, detail="") -> None:
    RESULTS.append({"name": name, "ok": False, "detail": detail})
    print(f"FAIL {name} :: {detail}", flush=True)


def check(name: str, cond: bool, detail="") -> bool:
    ok(name, detail) if cond else bad(name, detail)
    return bool(cond)


def info(name: str, detail="") -> None:
    """已确认的缺口/边界：记进报告，但不计入 PASS/FAIL（缺口另开卡，不把红涂抹掉）。"""
    RESULTS.append({"name": name, "ok": True, "info": True, "detail": detail})
    print(f"INFO {name} :: {detail}", flush=True)



# ------------------------------------------------------------------ HTTP
def http(method: str, path: str, body=None, timeout=30):
    url = API + path
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, method=method)
    if data is not None:
        req.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read().decode()
            return r.status, (json.loads(raw) if raw.strip() else None)
    except urllib.error.HTTPError as e:
        raw = e.read().decode()
        try:
            return e.code, json.loads(raw)
        except Exception:
            return e.code, {"raw": raw}


def get(path: str, timeout=30):
    return http("GET", path, None, timeout)


def post(path: str, body=None, timeout=60):
    return http("POST", path, {} if body is None else body, timeout)


def getj(path: str):
    st, body = get(path)
    return body if st == 200 else {"_status": st, "body": body}


# ------------------------------------------------------------------ DB
def db() -> sqlite3.Connection:
    conn = sqlite3.connect(str(DATA / "dam.db"), timeout=60)
    conn.row_factory = sqlite3.Row
    return conn


def scalar(sql: str, args=()):
    with db() as c:
        row = c.execute(sql, args).fetchone()
        return None if row is None else row[0]


def rows(sql: str, args=()):
    with db() as c:
        return [dict(r) for r in c.execute(sql, args).fetchall()]


# ------------------------------------------------------------------ 进程
PROC: subprocess.Popen | None = None


def kill_stray_apps() -> list[str]:
    """清掉**占着同一个端口**的遗留实例：遗留进程会占着端口应答健康检查，让「我刚起的那个」假就绪
    （第一版就吃过这个假象：首启判 PASS 但 DAM_WEB_DATA 里根本没库）。

    只认「同一个 DAM_WEB_PORT」的实例（卡 task）：同一容器里还有别的卡在跑 E2E，
    按进程名一刀切会把别人正在用的实例杀掉（实测把 sibling 卡的服务打挂）。

    并发例外（CMP_NO_KILL_STRAY=1，卡 task）：直接用 CMP_PORT / CMP_WORK / 独立 APP_BIN
    隔离，一个遗留实例都不动。
    """
    if os.environ.get("CMP_NO_KILL_STRAY") == "1":
        return []
    killed = []
    want_port = str(PORT)
    for d in Path("/proc").iterdir():
        if not d.name.isdigit() or d.name == str(os.getpid()):
            continue
        try:
            cmd = (d / "cmdline").read_bytes().split(b"\0")
        except OSError:
            continue
        if not cmd or not cmd[0]:
            continue
        if not os.path.basename(cmd[0].decode("utf-8", "replace")).startswith("app-web"):
            continue
        # 环境读不到 / 端口不是我的 → 是别人的实例，别动
        try:
            env = (d / "environ").read_bytes().split(b"\0")
        except OSError:
            continue
        port = None
        for kv in env:
            if kv.startswith(b"DAM_WEB_PORT="):
                port = kv.split(b"=", 1)[1].decode("utf-8", "replace")
                break
        if port != want_port:
            continue
        try:
            os.kill(int(d.name), signal.SIGKILL)
            killed.append(d.name)
        except OSError:
            pass
    if killed:
        time.sleep(1.5)
    return killed



def app_env() -> dict:
    env = dict(os.environ)
    env.update(
        DAM_WEB_DATA=str(DATA),
        DAM_WEB_PORT=str(PORT),
        DAM_WEB_DIST=str(DIST),
    )
    # VL 是内网直连（同网段 vLLM）：容器里的 http(s)_proxy 会把请求拐去代理 → 判定全失败
    for k in ("HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy", "ALL_PROXY", "all_proxy"):
        env.pop(k, None)
    return env


def start_app(wait=90) -> bool:
    global PROC
    log = open(WORK / "web.log", "ab")
    PROC = subprocess.Popen([APP], env=app_env(), cwd=str(WORK), stdout=log, stderr=log)
    t0 = time.time()
    while time.time() - t0 < wait:
        if PROC.poll() is not None:
            print(f"!! app-web 退出码 {PROC.returncode}（日志 {WORK/'web.log'}）", flush=True)
            return False
        try:
            st, _ = get("/api/library/stats", timeout=3)
            if st == 200:
                return True
        except Exception:
            pass
        time.sleep(0.5)
    return False


def stop_app(sig=signal.SIGTERM) -> None:
    global PROC
    if PROC and PROC.poll() is None:
        try:
            PROC.send_signal(sig)
        except ProcessLookupError:
            pass
        try:
            PROC.wait(timeout=20)
        except subprocess.TimeoutExpired:
            PROC.kill()
            PROC.wait(timeout=10)
    PROC = None


def kill_app_hard() -> None:
    """模拟进程崩溃（kill -9）：不给收尾机会 —— 要复现「run 停在 running 且没 finishedAt」。"""
    stop_app(signal.SIGKILL)


# ------------------------------------------------------------------ 判定驱动
def judge(scope="asset", **kw):
    body = {"all": True, "scope": scope}
    body.update(kw)
    return post("/api/compliance/judge", body)


def judge_status():
    return getj("/api/compliance/judge/status")


def wait_idle(timeout=900) -> bool:
    t0 = time.time()
    while time.time() - t0 < timeout:
        st = judge_status()
        if not st.get("running", True):
            return True
        time.sleep(1)
    return False


def wait_running(timeout=60) -> bool:
    t0 = time.time()
    while time.time() - t0 < timeout:
        if judge_status().get("running"):
            return True
        time.sleep(0.5)
    return False


def newest_run() -> dict:
    return (getj("/api/compliance/jobs?limit=1").get("jobs") or [{}])[0]


def run_detail(run_id: int, **kw) -> dict:
    qs = "&".join(f"{k}={v}" for k, v in kw.items())
    return getj(f"/api/compliance/jobs/{run_id}" + (f"?{qs}" if qs else ""))


# ------------------------------------------------------------------ 校验工具
INVARIANT_KEYS = ("runId", "status", "total", "done", "failed", "skipped", "cancelled", "finishedAt")


def snap(run: dict) -> dict:
    return {k: run.get(k) for k in INVARIANT_KEYS}


def db_counts(run_id: int) -> dict:
    with db() as c:
        total = c.execute(
            "SELECT count(*) FROM job_queue WHERE runId=?", (run_id,)
        ).fetchone()[0]
        done = c.execute(
            "SELECT count(*) FROM job_queue WHERE runId=? AND status='done'", (run_id,)
        ).fetchone()[0]
        failed = c.execute(
            "SELECT count(*) FROM job_queue WHERE runId=? AND status='failed'", (run_id,)
        ).fetchone()[0]
        pending = c.execute(
            "SELECT count(*) FROM job_queue WHERE runId=? AND status IN ('pending','running')",
            (run_id,),
        ).fetchone()[0]
        skipnote = c.execute(
            "SELECT count(*) FROM job_queue WHERE runId=? AND ("
            + " OR ".join(["lastError LIKE ?"] * len(SKIP_PREFIXES))
            + ")",
            (run_id, *[p + "%" for p in SKIP_PREFIXES]),
        ).fetchone()[0]
        groups = c.execute(
            "SELECT count(*) FROM job_queue WHERE runId=? AND kind='compliance_group'",
            (run_id,),
        ).fetchone()[0]
    return {
        "total": total,
        "done_rows": done,
        "failed": failed,
        "pending": pending,
        "skipnote": skipnote,
        "groups": groups,
    }


def assert_invariant(tag: str, run: dict) -> None:
    c = [run.get(k) or 0 for k in ("done", "failed", "skipped", "cancelled")]
    total = run.get("total") or 0
    check(
        f"{tag} 恒等式 total=done+failed+skipped+cancelled",
        total == sum(c),
        f"total={total} done={run.get('done')} failed={run.get('failed')} "
        f"skipped={run.get('skipped')} cancelled={run.get('cancelled')}",
    )


def assert_db_matches(tag: str, run: dict, expect_skipped_notes: int | None = None) -> None:
    d = db_counts(run["runId"])
    check(f"{tag} 队列行数==total", d["total"] == run.get("total"), f"db={d['total']} api={run.get('total')}")
    check(
        f"{tag} 队列 done 行 == done+skipped",
        d["done_rows"] == (run.get("done") or 0) + (run.get("skipped") or 0),
        f"db_done_rows={d['done_rows']} api done={run.get('done')} skipped={run.get('skipped')}",
    )
    check(f"{tag} 队列 failed 行==failed", d["failed"] == run.get("failed"), f"db={d['failed']} api={run.get('failed')}")
    check(
        f"{tag} 队列 pending/running 行==cancelled",
        d["pending"] == run.get("cancelled"),
        f"db_pending={d['pending']} api_cancelled={run.get('cancelled')}",
    )
    if expect_skipped_notes is not None:
        check(
            f"{tag} 跳过原因落库条数=={expect_skipped_notes}",
            d["skipnote"] == expect_skipped_notes,
            f"db not_image 行={d['skipnote']}",
        )


# ------------------------------------------------------------------ 造数
def seed_vl_settings() -> None:
    script = ROOT / "scripts/ui3_seed_settings.py"
    env = dict(os.environ, UI3_DATA=str(DATA), UI3_VL_BASE_URL=VL_BASE, UI3_VL_MODEL=VL_MODEL)
    out = subprocess.run([sys.executable, str(script)], env=env, capture_output=True, text=True)
    print(("seed settings: " + (out.stdout or out.stderr)).strip()[:400], flush=True)


def prepare_corpus() -> int:
    CORPUS.mkdir(parents=True, exist_ok=True)
    src = sorted(Path("/tmp/c5-stage").glob("*.jpg"))
    if len(src) < NIMG:
        src = sorted(Path("/tmp/c5-stage").glob("*.[jJ][pP][gG]"))
    for p in src[:NIMG]:
        shutil.copy2(p, CORPUS / p.name)
    return len(list(CORPUS.iterdir()))


def import_corpus() -> dict:
    post("/api/import/start", {"path": str(CORPUS), "storeMode": "copyin"})
    t0 = time.time()
    while time.time() - t0 < 300:
        st = getj("/api/import/status")
        if not st.get("running", True):
            break
        time.sleep(1)
    return getj("/api/library/stats")


def corrupt_files() -> dict:
    """BATCH-01 的真实形态：资产已入库、磁盘上的字节后来坏了/没了。

    坏字节 2 张（魔数不是图片 → 应 skipped）＋ 文件删掉 1 张（失联 → **不**该当跳过，
    元数据档规则还能判；这是「数据不一致要可见」的口径）。
    """
    with db() as c:
        assets = [
            dict(r)
            for r in c.execute(
                "SELECT assetId, absPath, fileName FROM assets ORDER BY assetId LIMIT 6"
            ).fetchall()
        ]
    junk = assets[0:2]
    gone = assets[2:3]
    for a in junk:
        Path(a["absPath"]).write_bytes(b"NOT-AN-IMAGE\x00\x01\x02 jpeg? no.\n" * 4)
    for a in gone:
        try:
            Path(a["absPath"]).unlink()
        except FileNotFoundError:
            pass
    return {
        "junk": [a["assetId"] for a in junk],
        "gone": [a["assetId"] for a in gone],
    }


def seed_legacy_rows(asset_ids: list[int]) -> int:
    """模拟「迁移前」的老行：runId IS NULL 的历史失败/成功任务（不回填、不进任何一次）。"""
    now = int(time.time())
    with db() as c:
        for i, aid in enumerate(asset_ids):
            c.execute(
                "INSERT INTO job_queue (kind, targetId, payload, status, attempts, lastError,"
                " createdAt, updatedAt, runId) VALUES ('compliance', ?, ?, ?, 3, ?, ?, ?, NULL)",
                (
                    aid,
                    json.dumps({"packId": "legacy"}),
                    "failed" if i < 3 else "done",
                    "legacy boom" if i < 3 else None,
                    now,
                    now,
                ),
            )
        c.commit()
        return c.execute("SELECT count(*) FROM job_queue WHERE runId IS NULL").fetchone()[0]


# ------------------------------------------------------------------ 主流程
def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default=str(ROOT / "reports/evidence/CMP-JOB-E2E"))
    ap.add_argument("--keep", action="store_true", help="保留上一轮的 data 目录（不重建）")
    args = ap.parse_args()
    out_dir = Path(args.out)
    out_dir.mkdir(parents=True, exist_ok=True)

    if not (DATA / "dam.db").exists() or not args.keep:
        shutil.rmtree(WORK, ignore_errors=True)
    WORK.mkdir(parents=True, exist_ok=True)
    stray = kill_stray_apps()
    if stray:
        print(f"清掉遗留 app-web 实例：{stray}", flush=True)

    # ---------- P0：起服务 + 造真数据 ----------
    print("========== P0 起服务 / 真图入库 / 造坏图 ==========", flush=True)
    if not (DATA / "dam.db").exists():
        DATA.mkdir(parents=True, exist_ok=True)
        booted = start_app()
        check("P0 app-web 首启（迁移建库）", booted and (DATA / "dam.db").exists(), str(DATA / "dam.db"))
        stop_app()
        seed_vl_settings()
        check(
            "P0 VL 设置已写入（settings.inference）",
            scalar("SELECT count(*) FROM settings WHERE key='inference'") == 1,
            "settings.inference",
        )
    check("P0 app-web 就绪（带 VL 设置）", start_app())
    check("P0 DAM_WEB_DATA 里真的有库（不是别的实例在应答）", (DATA / "dam.db").exists(), str(DATA / "dam.db"))

    stats = getj("/api/library/stats")
    if not stats.get("assets"):
        n_corpus = prepare_corpus()
        print(f"语料：{n_corpus} 张真图", flush=True)
        stats = import_corpus()
    assets_n = stats.get("assets") or 0
    check("P0 真图入库", assets_n >= NIMG, f"assets={assets_n} 期望>={NIMG}")

    with db() as c:
        all_ids = [r[0] for r in c.execute("SELECT assetId FROM assets ORDER BY assetId").fetchall()]
    legacy_n = scalar("SELECT count(*) FROM job_queue WHERE runId IS NULL")
    if not legacy_n:
        legacy_n = seed_legacy_rows(all_ids[:4])
    check("P0 迁移前老行已就位（runId IS NULL）", legacy_n == 4, f"legacy 行={legacy_n}")

    bad_facts = corrupt_files()
    junk_ok = all(
        not Path(r["absPath"]).read_bytes().startswith(b"\xff\xd8") for r in rows(
            "SELECT absPath FROM assets WHERE assetId IN (%s)" % ",".join("?" * len(bad_facts["junk"])),
            bad_facts["junk"],
        )
    )
    check("P0 坏字节已写入（魔数不是 JPEG）", junk_ok, f"assetIds={bad_facts['junk']}")
    check(
        "P0 失联文件已删除（DB 里仍在）",
        not Path(rows("SELECT absPath FROM assets WHERE assetId=?", bad_facts["gone"][:1])[0]["absPath"]).exists(),
        f"assetId={bad_facts['gone']}",
    )

    # ---------- P1：run1 真跑完（asset 口径）→ 跳过要看得到、不算失败 ----------
    print("\n========== P1 run1：真跑完（含 2 张坏图） ==========", flush=True)
    st, body = judge("asset")
    check("P1 judge 已受理", st == 200, f"http={st} {str(body)[:120]}")
    check("P1 run1 收工", wait_idle(), "")
    run1 = newest_run()
    r1 = run1.get("runId")
    print(json.dumps(snap(run1), ensure_ascii=False), flush=True)
    check("P1 run1 状态 done（跑完 ≠ 每张都成功）", run1.get("status") == "done", run1.get("statusLabel"))
    check("P1 run1 total==入库张数", run1.get("total") == assets_n, f"total={run1.get('total')} assets={assets_n}")
    check("P1 run1 skipped==2（坏字节被跳过，不是失败）", run1.get("skipped") == 2, f"skipped={run1.get('skipped')}")
    check(
        "P1 历史累计（runId NULL）没有掺进本次",
        run1.get("total") == assets_n and legacy_n == 4,
        f"run1.total={run1.get('total')} legacy={legacy_n}",
    )
    assert_invariant("P1", run1)
    assert_db_matches("P1", run1, expect_skipped_notes=2)

    st = judge_status()
    legacy_failed = scalar("SELECT count(*) FROM job_queue WHERE kind='compliance' AND status='failed' AND runId IS NULL")
    cur = st.get("queueCurrent") or {}
    check("P1 /judge/status 标了 legacyCumulative", st.get("legacyCumulative") is True, str(st.get("legacyCumulative")))
    check(
        "P1 /judge/status queue.assetFailed 是累计口径（历史失败仍在）",
        (st.get("queue") or {}).get("assetFailed") == legacy_failed + (run1.get("failed") or 0),
        f"queue.assetFailed={(st.get('queue') or {}).get('assetFailed')} legacy_failed={legacy_failed} run1.failed={run1.get('failed')}",
    )
    check(
        "P1 queueCurrent 只算本次（assetFailed==本次 failed）",
        cur.get("assetFailed") == (run1.get("failed") or 0) and cur.get("runId") == r1,
        f"queueCurrent={cur}",
    )
    check(
        "P1 status.run 指向本次 run",
        (st.get("run") or {}).get("runId") == r1,
        f"run.runId={(st.get('run') or {}).get('runId')} 期望 {r1}",
    )

    detail = run_detail(r1)
    tasks_all = detail.get("tasks") or []
    check("P1 明细 taskTotal==total", detail.get("taskTotal") == run1.get("total"), f"taskTotal={detail.get('taskTotal')}")
    skip_tasks = [t for t in tasks_all if (t.get("lastError") or "").startswith("not_image:")]
    check(
        "P1 明细里能解释跳过（status=done + not_image 原因）",
        len(skip_tasks) == 2 and all(t.get("status") == "done" for t in skip_tasks),
        f"跳过 task={[(t['jobId'], t['status']) for t in skip_tasks]}",
    )
    check(
        "P1 步骤名是人话（不是 job123 / group47）",
        all(re.match(r"^(逐图判定|组级判定)", (t.get("step") or "")) for t in tasks_all) and len(tasks_all) > 0,
        f"steps={[t.get('step') for t in tasks_all[:3]]}",
    )
    page1 = run_detail(r1, limit=3, offset=0).get("tasks") or []
    page2 = run_detail(r1, limit=3, offset=3).get("tasks") or []
    ids1 = [t["jobId"] for t in page1]
    ids2 = [t["jobId"] for t in page2]
    want1, want2 = min(3, run1["total"]), min(3, max(0, run1["total"] - 3))
    check(
        "P1 明细分页 offset 真生效（页间不重叠、条数正确）",
        len(ids1) == want1 and len(ids2) == want2 and not set(ids1) & set(ids2),
        f"p1={ids1} p2={ids2}（期望 {want1}/{want2}，total={run1['total']}）",
    )
    st404, body404 = get(f"/api/compliance/jobs/999999")
    check("P1 不存在的 run → 404 run_not_found", st404 == 404 and (body404 or {}).get("error") == "run_not_found", f"http={st404} {body404}")
    run1_snap = snap(run1)

    # ---------- P2：run2 再来一次（同口径）→ 各次分开 ----------
    print("\n========== P2 run2：同口径再跑一次 ==========", flush=True)
    st, _ = judge("asset")
    check("P2 judge 已受理", st == 200 or st == 409, f"http={st}")
    if wait_idle():
        run2 = newest_run()
        r2 = run2.get("runId")
        print(json.dumps(snap(run2), ensure_ascii=False), flush=True)
        check("P2 第二次执行是独立 run（不覆盖上一次）", r2 and r2 != r1, f"run1={r1} run2={r2}")
        check("P2 run2 状态 done", run2.get("status") == "done", run2.get("statusLabel"))
        d2 = db_counts(r2)
        check(
            "P2 run2 total == 本 run 的队列行数",
            run2.get("total") == d2["total"],
            f"api={run2.get('total')} db={d2['total']}",
        )
        check("P2 run2 skipped==2（每次独立记账，坏图照样跳过）", run2.get("skipped") == 2, f"skipped={run2.get('skipped')}")
        check(
            "P2 run2 的失败只算本次（两次各自失败数互不叠加）",
            run2.get("failed") == d2["failed"],
            f"api={run2.get('failed')} db={d2['failed']} run1.failed={run1.get('failed')}",
        )
        assert_invariant("P2", run2)
        assert_db_matches("P2", run2, expect_skipped_notes=2)
        r1_now = snap((run_detail(r1).get("run") or {}))
        check("P2 run1 的数字在 run2 之后完全没变", r1_now == run1_snap, f"before={run1_snap} after={r1_now}")
        st2 = judge_status()
        cur2 = st2.get("queueCurrent") or {}
        check(
            "P2 queueCurrent 跟着跑到本次",
            cur2.get("runId") == r2 and cur2.get("assetFailed") == (run2.get("failed") or 0),
            f"queueCurrent={cur2}",
        )
    else:
        bad("P2 run2 收工", "超时")

    # ---------- P2b：组级任务（**内置包**路径；realestate 自卡 task 起已编译期内嵌） ----------
    # 这里不再 import：P2b 曾经靠 `packs/import {dir:"<宿主绝对路径>"}` 换包，等于把「组级任务」
    # 的成立条件挂在一个手工导入前置上。现在 realestate 是内置包（Pack::load_builtin），
    # P2b 直接激活内置包 —— 零前置、零宿主路径依赖。
    if os.environ.get("CMP_SKIP_GROUP") == "1":
        print("\n========== P2b 跳过（CMP_SKIP_GROUP=1） ==========", flush=True)
        run_group = None
    else:
        print("\n========== P2b 组级任务：内置 realestate 包（含 per_group 规则） ==========", flush=True)
        pk = getj("/api/compliance/packs") or {}
        builtin_packs = [b for b in (pk.get("builtin") or []) if isinstance(b, dict)]  # {activePack, builtin:[{id,...}], imported:[]}
        builtin_ids = [b.get("id") for b in builtin_packs]
        check(
            "P2b 内置清单里有带 per_group 规则的包（realestate，不靠 import）",
            "realestate" in builtin_ids,
            f"builtin={builtin_ids}",
        )
        re_b = next((b for b in builtin_packs if b.get("id") == "realestate"), {})
        check(
            "P2b 内置 realestate 装载无错（error=null，rules>0）",
            re_b.get("error") is None and (re_b.get("rules") or 0) > 0,
            json.dumps(re_b, ensure_ascii=False)[:180],
        )
        st, body = post("/api/compliance/packs/activate", {"source": "builtin", "id": "realestate"})
        act = (body.get("activePack") if isinstance(body, dict) else None) or {}
        check(
            "P2b 激活内置 realestate（source=builtin，无 import、无人工前置）",
            st == 200 and act.get("id") == "realestate" and act.get("source") == "builtin",
            f"http={st} activePack={act}",
        )
        st, _ = judge("both")
        check("P2b group judge 已受理", st == 200, f"http={st}")
        if wait_idle(1800):
            rg = newest_run()
            rid = rg.get("runId")
            print(json.dumps(snap(rg), ensure_ascii=False), flush=True)
            dg = db_counts(rid)
            check("P2b 组级任务真的入队了（per_group 规则 → 一组一个 task）", dg["groups"] > 0, f"group jobs={dg['groups']}")
            check(
                "P2b 组级执行的 total 把组级任务算进本次（不是另开一次执行）",
                rg.get("total") == dg["total"],
                f"api.total={rg.get('total')} db.total={dg['total']}（逐图 {dg['total'] - dg['groups']} + 组级 {dg['groups']}）",
            )
            check("P2b 组级执行独立成行", rid not in (r1, r2), f"run={rid}")
            assert_invariant("P2b", rg)
            assert_db_matches("P2b", rg)
            check(
                "P2b 前面几次执行在组级跑完后数字不变",
                snap(run_detail(r1).get("run") or {}) == run1_snap
                and snap(run_detail(r2).get("run") or {}) == snap(run2),
                "",
            )
            stq = judge_status().get("queueCurrent") or {}
            check(
                "P2b queueCurrent 里组级计数非零（界面据它显示组级进度）",
                (stq.get("groupDone") or 0) + (stq.get("groupFailed") or 0) > 0,
                f"queueCurrent={stq}",
            )
            run_group = snap(rg)
        else:
            bad("P2b 组级执行收工", "超时")
            run_group = None
        # 回到内置包（后面的取消/中断/认领场景沿用 P1 口径，便于跨次比较）
        post("/api/compliance/packs/activate", {"source": "builtin", "id": "ecommerce-taobao"})
        check(
            "P2b 已切回内置包",
            (getj("/api/compliance/packs").get("activePack") or {}).get("id") == "ecommerce-taobao",
            "",
        )



    # ---------- P3：run3 取消 ----------
    print("\n========== P3 run3：跑到一半取消 ==========", flush=True)
    judge("asset")
    wait_running()
    time.sleep(2)
    # 运行中的口径：进度必须真的在动，且与队列事实一致（防「假进度」—— 用行快照当实时计数）
    live = None
    t0 = time.time()
    while time.time() - t0 < 120:
        cur = (getj("/api/compliance/jobs/current") or {}).get("run") or {}
        if (cur.get("done") or 0) + (cur.get("failed") or 0) + (cur.get("skipped") or 0) >= 1:
            live = cur
            break
        time.sleep(2)
    if live is None:
        bad("P3 运行中进度可见", "120s 内没看到任何已处理条数（进度条会一直是 0）")
    else:
        dl = db_counts(live["runId"])
        check(
            "P3 运行中的进度与队列事实一致（已处理 = 队列 done 行 + failed 行）",
            (live.get("done") or 0) + (live.get("failed") or 0) + (live.get("skipped") or 0)
            == dl["done_rows"] + dl["failed"],
            f"api done={live.get('done')} failed={live.get('failed')} skipped={live.get('skipped')} "
            f"db done_rows={dl['done_rows']} failed_rows={dl['failed']}",
        )
        check(
            "P3 运行中的 total 与队列行数一致（进度条分母不虚）",
            live.get("total") == dl["total"],
            f"api.total={live.get('total')} db.total={dl['total']}",
        )
        # GAP-2（卡 task）：运行中的 cancelled 必须为 0，排队中的条数走 queued。
        # 以前 run_view 复用 live.cancelled()（= pending），界面会把「还在排队」读成「未执行」。
        check(
            "P3 运行中 cancelled 恒为 0（不再复用 pending）",
            (live.get("cancelled") or 0) == 0,
            f"runId={live.get('runId')} cancelled={live.get('cancelled')} queued={live.get('queued')}",
        )
        check(
            "P3 运行中 queued == total-(done+failed+skipped)（未结算条数独立字段）",
            (live.get("queued") or 0)
            == (live.get("total") or 0)
            - ((live.get("done") or 0) + (live.get("failed") or 0) + (live.get("skipped") or 0)),
            f"total={live.get('total')} done={live.get('done')} failed={live.get('failed')} "
            f"skipped={live.get('skipped')} queued={live.get('queued')}",
        )
        # 与 DB 对账（再取一次 current，缩小轮询与查库之间的窗口）
        queued_api = ((getj("/api/compliance/jobs/current") or {}).get("run") or {}).get("queued") or 0
        check(
            "P3 运行中 queued 与队列 pending/running 行一致",
            queued_api == dl["pending"],
            f"api.queued={queued_api} db_pending={dl['pending']}",
        )
        curq = (getj("/api/compliance/jobs/current") or {}).get("queue") or {}
        check(
            "P3 queueCurrent.queued == assetPending+groupPending 且 cancelled=0",
            (curq.get("queued") or 0) == (curq.get("assetPending") or 0) + (curq.get("groupPending") or 0)
            and (curq.get("cancelled") or 0) == 0,
            f"queueCurrent={curq}",
        )
    st, body = post("/api/compliance/judge/cancel", {})
    check("P3 取消已受理", st == 200, f"http={st} {body}")
    check("P3 run3 收工", wait_idle(), "")
    run3 = newest_run()
    r3 = run3.get("runId")
    print(json.dumps(snap(run3), ensure_ascii=False), flush=True)
    check("P3 run3 状态 cancelled", run3.get("status") == "cancelled", run3.get("statusLabel"))
    check("P3 run3 有未执行任务（取消≠跑完）", (run3.get("cancelled") or 0) >= 1, f"cancelled={run3.get('cancelled')}")
    check("P3 run3 有耗时（finishedAt 已落）", run3.get("finishedAt") and (run3.get("durationMs") or 0) >= 0, f"finishedAt={run3.get('finishedAt')} durationMs={run3.get('durationMs')}")
    assert_invariant("P3", run3)
    assert_db_matches("P3", run3)
    pend = scalar("SELECT count(*) FROM job_queue WHERE runId=? AND status IN ('pending','running')", (r3,))
    check("P3 取消后剩余任务仍在队列（断点续跑，不删不改判）", pend >= 1, f"pending rows={pend}")
    check("P3 run2 在 run3 之后没变", snap(run_detail(r2).get("run") or {}) == snap(run2), "")
    run3_snap = snap(run3)

    # ---------- P4：run4 崩溃中断 ----------
    print("\n========== P4 run4：跑到一半 kill -9 → 重启自愈 ==========", flush=True)
    judge("asset")
    wait_running()
    time.sleep(2)
    kill_app_hard()
    check("P4 崩溃后重启就绪", start_app())
    time.sleep(2)
    run4 = newest_run()
    r4 = run4.get("runId")
    print(json.dumps(snap(run4), ensure_ascii=False), flush=True)
    check("P4 崩溃那次被启动自愈成 interrupted", run4.get("status") == "interrupted", run4.get("statusLabel"))
    check("P4 interrupted 有收工时间（不是永远转圈）", bool(run4.get("finishedAt")), f"finishedAt={run4.get('finishedAt')}")
    assert_invariant("P4", run4)
    check(
        "P4 库里没有残留 running 的 run",
        scalar("SELECT count(*) FROM compliance_runs WHERE status='running'") == 0,
        f"running runs={scalar('SELECT count(*) FROM compliance_runs WHERE status=?', ('running',))}",
    )
    check("P4 /jobs/current 无在跑执行（run=null）", (getj("/api/compliance/jobs/current") or {}).get("run") is None, "")
    check("P4 run3 在重启后没被改写", snap(run_detail(r3).get("run") or {}) == run3_snap, "")
    run4_snap = snap(run4)

    # ---------- P5：run5 认领上一轮遗留的 pending ----------
    print("\n========== P5 run5：下一次执行认领遗留任务 ==========", flush=True)
    left_ids = [r["id"] for r in rows("SELECT id FROM job_queue WHERE runId IN (?,?) AND status IN ('pending','running')", (r3, r4))]
    print(f"run3/run4 遗留 pending task：{len(left_ids)} 条", flush=True)
    judge("asset")
    check("P5 run5 收工", wait_idle(), "")
    run5 = newest_run()
    r5 = run5.get("runId")
    print(json.dumps(snap(run5), ensure_ascii=False), flush=True)
    check("P5 第三次执行独立成行", r5 and len({r1, r2, r3, r4, r5}) == 5, f"runs={[r1,r2,r3,r4,r5]}")
    check("P5 run5 状态 done", run5.get("status") == "done", run5.get("statusLabel"))
    adopted = [i for i in left_ids if scalar("SELECT runId FROM job_queue WHERE id=?", (i,)) == r5]
    check(
        "P5 上一轮遗留任务被认领进本次（不丢、不重复成独立 run）",
        len(left_ids) == 0 or len(adopted) == len(left_ids),
        f"遗留={len(left_ids)} 被认领={len(adopted)}",
    )
    check("P5 run5 total 覆盖认领部分", (run5.get("total") or 0) >= assets_n, f"total={run5.get('total')} assets={assets_n}")
    # 跳过计数：坏字节 2 条有 not_image 原因；失联那张在重启后被 watcher 打了 missing=1，
    # 走 process_job 的「资产状态」跳过分支（同属跳过口径，但不写原因）→ 故用 >= 而不是 ==。
    check(
        "P5 run5 的跳过只增不减（坏字节 + 缺失标记资产都算跳过，不进失败）",
        (run5.get("skipped") or 0) >= 2 and (run5.get("failed") or 0) == db_counts(r5)["failed"],
        f"skipped={run5.get('skipped')} failed={run5.get('failed')}",
    )
    assert_invariant("P5", run5)
    # CMP-JOB-FIX-1 后的硬口径：每条计入 skipped 的 task 都必须带跳过原因，
    # 所以「落库原因条数」= skipped，不再是「>= 2」。
    assert_db_matches("P5", run5, expect_skipped_notes=run5.get("skipped") or 0)
    n_skipped = run5.get("skipped") or 0
    detail5 = run_detail(r5)
    tasks5 = detail5.get("tasks") or []
    reasoned = [t for t in tasks5 if is_skip_reason(t.get("lastError"))]
    check(
        "P5 不变量：run.skipped == 明细里带跳过原因的 task 数",
        n_skipped == len(reasoned),
        f"run.skipped={n_skipped} 带原因={len(reasoned)} 前缀={[ (t.get('lastError') or '')[:22] for t in reasoned ]}",
    )
    # 失联那张（watcher 打了 missing=1）必须自己解释自己：前缀 file_missing:
    gone_id = bad_facts["gone"][0]
    gone_task = next((t for t in tasks5 if t.get("targetId") == gone_id), None)
    check(
        "P5 失联资产（missing=1）的跳过错原因前缀是 file_missing:",
        gone_task is not None and (gone_task.get("lastError") or "").startswith("file_missing:"),
        f"asset{gone_id} task={(gone_task or {}).get('jobId')} lastError={(gone_task or {}).get('lastError')!r}",
    )
    # 界面口径钉子：不能再有「done + 非跳过原文」这种无原因跳过（上一轮的失败原文必须被覆盖）
    ghosts = [
        t
        for t in tasks5
        if t.get("status") == "done"
        and (t.get("lastError") or "").strip()
        and not is_skip_reason(t.get("lastError"))
    ]
    check(
        "P5 明细里没有「done + 残留失败原文」的无人认领行（跳过原因覆盖旧错误）",
        len(ghosts) == 0,
        f"无原因残留={[(t.get('jobId'), (t.get('lastError') or '')[:40]) for t in ghosts[:3]]}",
    )

    check(
        "P5 认领后 run3/run4 的历史行数字不变（认领改队列，不改历史账）",
        snap(run_detail(r3).get("run") or {}) == run3_snap and snap(run_detail(r4).get("run") or {}) == run4_snap,
        f"r3={snap(run_detail(r3).get('run') or {})} r4={snap(run_detail(r4).get('run') or {})}",
    )
    check(
        "P5 认领后 run3/run4 名下已无 pending/running 行",
        scalar("SELECT count(*) FROM job_queue WHERE runId IN (?,?) AND status IN ('pending','running')", (r3, r4)) == 0,
        "",
    )

    # ---------- P6：全局口径 ----------
    print("\n========== P6 全局口径对账 ==========", flush=True)
    alljobs = getj("/api/compliance/jobs?limit=100")
    jobs = alljobs.get("jobs") or []
    check("P6 执行列表条数==compliance_runs 行数", alljobs.get("total") == scalar("SELECT count(*) FROM compliance_runs"), f"api={alljobs.get('total')} db={scalar('SELECT count(*) FROM compliance_runs')}")
    check("P6 执行列表按开始时间倒序", all(jobs[i]["startedAt"] >= jobs[i + 1]["startedAt"] for i in range(len(jobs) - 1)), f"startedAt={[j['startedAt'] for j in jobs]}")
    all_inv = all(
        (j.get("total") or 0)
        == (j.get("done") or 0) + (j.get("failed") or 0) + (j.get("skipped") or 0) + (j.get("cancelled") or 0)
        for j in jobs
    )
    check("P6 每次执行的恒等式成立", all_inv, f"n={len(jobs)}")
    orphans = scalar(
        "SELECT count(*) FROM job_queue q WHERE q.runId IS NOT NULL AND NOT EXISTS"
        " (SELECT 1 FROM compliance_runs r WHERE r.runId = q.runId)"
    )
    check("P6 没有挂在不存在的 run 上的 task", orphans == 0, f"orphan={orphans}")
    check(
        "P6 迁移前老行仍是 NULL（未回填，历史失败不会在新 run 里复活）",
        scalar("SELECT count(*) FROM job_queue WHERE runId IS NULL") == 4,
        f"legacy={scalar('SELECT count(*) FROM job_queue WHERE runId IS NULL')}",
    )
    dup = scalar(
        "SELECT count(*) FROM (SELECT id FROM job_queue GROUP BY id HAVING count(*) > 1)"
    )
    check("P6 task id 唯一（无重复计入）", dup == 0, f"dup={dup}")
    p = getj("/api/compliance/jobs?limit=2&offset=0")
    p2 = getj("/api/compliance/jobs?limit=2&offset=2")
    check(
        "P6 列表分页 offset 真生效",
        [j["runId"] for j in (p.get("jobs") or [])] == [j["runId"] for j in jobs[:2]]
        and [j["runId"] for j in (p2.get("jobs") or [])] == [j["runId"] for j in jobs[2:4]],
        f"p1={[j['runId'] for j in (p.get('jobs') or [])]} p2={[j['runId'] for j in (p2.get('jobs') or [])]} all={[j['runId'] for j in jobs]}",
    )

    detail_r1 = run_detail(r1)
    stop_app()

    # ---------- 落盘 ----------
    passed = sum(1 for r in RESULTS if r["ok"] and not r.get("info"))
    infos = sum(1 for r in RESULTS if r.get("info"))
    failed = sum(1 for r in RESULTS if not r["ok"])
    summary = {
        "generatedAt": int(time.time()),
        "port": PORT,
        "images": assets_n,
        "vl": VL_BASE,
        "runs": [r for r in [r1, r2, r3, r4, r5] if r],
        "passed": passed,
        "failed": failed,
        "infos": infos,
        "assertions": RESULTS,
    }
    (out_dir / "assertions.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2))
    # 给 UI 探针留一份 API 快照：界面数字必须与这里逐字对上
    (out_dir / "api-snapshot.json").write_text(
        json.dumps({"jobs": jobs, "detailRun1": detail_r1}, ensure_ascii=False, indent=2)
    )
    print(f"\n==== E2E 断言：{passed} PASS / {failed} FAIL / {infos} INFO —— 证据 {out_dir} ====", flush=True)
    return failed


if __name__ == "__main__":
    sys.exit(main())
