#!/usr/bin/env python3
"""WEBCAP 验收（卡 t_43212e61）：服务端能力开关 + 「移动到目录」前置拦截的 HTTP 断言。

在**容器内**跑（下面常量都是容器路径；容器必须挂一个可写卷 `/data` 和一个
**只读且不同设备**的 `/corpus`，否则测不出 `read_only_fs` / `cross_device` 的真值）：

    python3 scripts/webcap_http_asserts.py --phase seed
    python3 scripts/webcap_http_asserts.py --base http://127.0.0.1:8891 --form web --phase flags
    python3 scripts/webcap_http_asserts.py --base http://127.0.0.1:8891 --form web --phase reject
    python3 scripts/webcap_http_asserts.py --base http://127.0.0.1:8891 --form web --phase move
    python3 scripts/webcap_http_asserts.py --base http://127.0.0.1:8892 --form desktop --phase flags

退出码 0 = 本阶段全 PASS。逐条打印 `PASS/FAIL <case> :: <原始返回>`（报告直接摘原文）。
"""
import argparse
import json
import os
import shutil
import sqlite3
import sys
import urllib.error
import urllib.request

DATA = "/data/webcap-webdata"      # DAM_WEB_DATA（可写卷：db/缩略图）
LIB = "/data/webcap-lib"           # 可写库根（导入批次根路径）
CORPUS = "/corpus/batch1"          # 只读挂载 + 与 /data 不同设备（跨设备真值）
OUTSIDE = "/tmp/webcap-outside"    # 可写但**不在**任何库根内
DB = os.path.join(DATA, "dam.db")

PASS = 0
FAIL = 0


def check(name, ok, detail=""):
    global PASS, FAIL
    if ok:
        PASS += 1
        print(f"PASS {name} :: {detail}")
    else:
        FAIL += 1
        print(f"FAIL {name} :: {detail}")
    return ok


def http(method, url, body=None, timeout=20):
    data = None
    headers = {}
    if body is not None:
        data = json.dumps(body).encode()
        headers["content-type"] = "application/json"
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read().decode("utf-8", "replace")
            return r.status, raw
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")


def jget(url):
    st, raw = http("GET", url)
    try:
        return st, json.loads(raw)
    except Exception:
        return st, {"_raw": raw}


def jpost(url, body):
    st, raw = http("POST", url, body)
    try:
        return st, json.loads(raw)
    except Exception:
        return st, {"_raw": raw}


# ---------- seed ----------

def phase_seed():
    """造一个真库：两个库根（可写 /data/webcap-lib、只读 /corpus/batch1），各 2 张真图。"""
    os.makedirs(LIB, exist_ok=True)
    srcs = sorted(f for f in os.listdir(CORPUS) if f.lower().endswith(".jpg"))[:2]
    check("语料下限（/corpus/batch1 至少 2 张 jpg）", len(srcs) == 2, f"{srcs}")
    lib_names = []
    for i, s in enumerate(srcs):
        name = f"lib-{i}.jpg"
        shutil.copyfile(os.path.join(CORPUS, s), os.path.join(LIB, name))
        lib_names.append(name)

    conn = sqlite3.connect(DB)
    conn.execute("DELETE FROM assets")
    conn.execute("DELETE FROM import_batches")
    now = 1_700_000_000
    for root in (CORPUS, LIB):
        conn.execute(
            "INSERT INTO import_batches (rootPath, storeMode, status, totalFiles, doneFiles, startedAt, endedAt)"
            " VALUES (?, 'reference', 'done', 2, 2, ?, ?)",
            (root, now, now),
        )
    rows = [(os.path.join(CORPUS, s), s) for s in srcs] + [
        (os.path.join(LIB, n), n) for n in lib_names
    ]
    ids = []
    for abs_path, name in rows:
        cur = conn.execute(
            "INSERT INTO assets (mediaType, absPath, fileName, fileExt, fileSz, mtime, importedAt)"
            " VALUES ('image', ?, ?, 'jpg', ?, ?, ?)",
            (abs_path, name, os.path.getsize(abs_path), now, now),
        )
        ids.append(cur.lastrowid)
    conn.commit()
    conn.close()
    ids_file = os.path.join(DATA, "webcap-ids.json")
    spec = {
        "corpus_ids": ids[:2],
        "data_ids": ids[2:],
        "corpus_dir": CORPUS,
        "data_dir": LIB,
        "outside": OUTSIDE,
    }
    with open(ids_file, "w") as f:
        json.dump(spec, f, ensure_ascii=False)
    check("seed 完成（2 只读根资产 + 2 可写根资产）", len(ids) == 4, f"ids={ids} spec={ids_file}")
    return 0 if FAIL == 0 else 1


def load_spec():
    with open(os.path.join(DATA, "webcap-ids.json")) as f:
        return json.load(f)


# ---------- flags ----------

def assert_caps_shape(caps, form):
    feats = caps.get("features") or {}
    reasons = caps.get("featureReasons") or {}
    roots = {r["path"]: r for r in (caps.get("roots") or [])}
    lib_dev = roots.get(LIB, {}).get("device")
    corpus = roots.get(CORPUS, {})

    check(
        f"[{form}] capabilities 结构齐全",
        {"isDesktop", "storeMode", "writableRoots", "roots", "features", "featureReasons"}
        <= set(caps.keys()),
        json.dumps(sorted(caps.keys())),
    )
    check(
        f"[{form}] features 键名与契约一致（前端按名字读，改名=静默失效）",
        set(feats.keys()) == {"moveAsset", "renameAsset", "relink", "openInOS", "physicalDelete", "copyInImport"},
        json.dumps(sorted(feats.keys())),
    )
    check(
        f"[{form}] roots 含两个库根（可写 / 只读）",
        set(roots) >= {DATA, LIB, CORPUS},
        json.dumps(sorted(roots)),
    )
    check(
        f"[{form}] /data/webcap-lib 实测可写",
        roots.get(LIB, {}).get("writable") is True,
        json.dumps(roots.get(LIB), ensure_ascii=False),
    )
    check(
        f"[{form}] /corpus/batch1 实测不可写且归类 read_only_fs",
        corpus.get("writable") is False and corpus.get("code") == "read_only_fs",
        json.dumps(corpus, ensure_ascii=False),
    )
    check(
        f"[{form}] /corpus 与 /data 设备号不同（跨设备真值，不是猜的）",
        corpus.get("device") is not None and lib_dev is not None and corpus.get("device") != lib_dev,
        f"corpus.device={corpus.get('device')} lib.device={lib_dev}",
    )
    check(
        f"[{form}] writableRoots 只含可写根（不含 /corpus/batch1）",
        CORPUS not in caps.get("writableRoots", [])
        and LIB in caps.get("writableRoots", [])
        and DATA in caps.get("writableRoots", []),
        json.dumps(caps.get("writableRoots"), ensure_ascii=False),
    )
    return feats, reasons


def phase_flags(base, form):
    st, caps = jget(f"{base}/api/capabilities")
    if not check(f"[{form}] GET /api/capabilities 200", st == 200, str(st)):
        return 1
    feats, reasons = assert_caps_shape(caps, form)

    if form == "web":
        check("[web] isDesktop=false / storeMode=server-volume",
              caps.get("isDesktop") is False and caps.get("storeMode") == "server-volume",
              f"isDesktop={caps.get('isDesktop')} storeMode={caps.get('storeMode')}")
        check(
            "[web] 磁盘类能力全部关闭（move/rename/relink/openInOS/physicalDelete）",
            all(feats.get(k) is False for k in
                ("moveAsset", "renameAsset", "relink", "openInOS", "physicalDelete")),
            json.dumps(feats, ensure_ascii=False),
        )
        check("[web] copyInImport 仍可用（复制入库不受形态限制）",
              feats.get("copyInImport") is True, json.dumps(feats, ensure_ascii=False))
        reason = reasons.get("moveAsset", "")
        check("[web] moveAsset 有非空人话 reason（指向桌面版）",
              len(reason) > 10 and "桌面版" in reason, reason)
        check("[web] 可用能力不带 reason（只填 false 的项）",
              "copyInImport" not in reasons, json.dumps(reasons, ensure_ascii=False))
    else:
        check("[desktop] isDesktop=true / storeMode=local-disk",
              caps.get("isDesktop") is True and caps.get("storeMode") == "local-disk",
              f"isDesktop={caps.get('isDesktop')} storeMode={caps.get('storeMode')}")
        check(
            "[desktop] 磁盘类能力全部打开（真值来自可写根实测）",
            all(feats.get(k) is True for k in
                ("moveAsset", "renameAsset", "relink", "openInOS", "physicalDelete")),
            json.dumps(feats, ensure_ascii=False),
        )
        check("[desktop] 全能力可用 → 无 moveAsset reason",
              "moveAsset" not in reasons, json.dumps(reasons, ensure_ascii=False))
    return 0 if FAIL == 0 else 1


# ---------- reject（前置拦截） ----------

def phase_reject(base, form):
    spec = load_spec()
    corpus_ids = spec["corpus_ids"]
    data_ids = spec["data_ids"]

    # 1) 只读根作目标 → read_only_fs（fs 归因，不是「等 rename 报错」）
    st, body = jpost(f"{base}/api/library/move",
                     {"ids": corpus_ids, "targetDir": f"{CORPUS}/sub"})
    check(f"[{form}] 只读根目标前置拒绝 400 + read_only_fs",
          st == 400 and body.get("code") == "read_only_fs", f"{st} {json.dumps(body, ensure_ascii=False)}")
    check(f"[{form}] 只读根拒绝带人话 error", len(body.get("error") or "") > 10, body.get("error", ""))
    check(f"[{form}] 只读根拒绝未创建目标目录（无磁盘副作用）",
          not os.path.exists(f"{CORPUS}/sub"), f"{CORPUS}/sub exists={os.path.exists(f'{CORPUS}/sub')}")

    # 2) 跨设备（源在 /corpus，目标在可写根 /data）→ cross_device
    st, body = jpost(f"{base}/api/library/move",
                     {"ids": corpus_ids, "targetDir": f"{LIB}/moved-xdev"})
    check(f"[{form}] 跨设备目标前置拒绝 400 + cross_device",
          st == 400 and body.get("code") == "cross_device", f"{st} {json.dumps(body, ensure_ascii=False)}")
    check(f"[{form}] 跨设备拒绝未创建目标目录（无磁盘副作用）",
          not os.path.exists(f"{LIB}/moved-xdev"), f"{LIB}/moved-xdev exists={os.path.exists(f'{LIB}/moved-xdev')}")

    # 3) 可写但不在任何库根内 → not_writable_root
    os.makedirs(OUTSIDE, exist_ok=True)
    st, body = jpost(f"{base}/api/library/move",
                     {"ids": data_ids, "targetDir": OUTSIDE})
    check(f"[{form}] 库根外目标前置拒绝 400 + not_writable_root",
          st == 400 and body.get("code") == "not_writable_root", f"{st} {json.dumps(body, ensure_ascii=False)}")

    # 4) 相对路径 → bad_path
    st, body = jpost(f"{base}/api/library/move", {"ids": data_ids, "targetDir": "rel/dir"})
    check(f"[{form}] 相对路径前置拒绝 400 + bad_path",
          st == 400 and body.get("code") == "bad_path", f"{st} {json.dumps(body, ensure_ascii=False)}")

    # 5) 拒绝后库内 absPath 一个都没变（前置拦截 = 零副作用）
    st, listing = jget(f"{base}/api/library/assets?ids=" + ",".join(str(i) for i in corpus_ids + data_ids))
    paths = {it["assetId"]: it["absPath"] for it in (listing.get("items") or [])}
    unchanged = all(
        paths.get(i, "").startswith((CORPUS, LIB)) for i in corpus_ids + data_ids
    )
    check(f"[{form}] 所有前置拒绝后 absPath 未变", unchanged, json.dumps(paths, ensure_ascii=False))
    return 0 if FAIL == 0 else 1


# ---------- move（② 不回归） ----------

def phase_move(base, form):
    spec = load_spec()
    data_ids = spec["data_ids"]
    target = f"{LIB}/moved"
    st, body = jpost(f"{base}/api/library/move", {"ids": data_ids, "targetDir": target})
    ok = check(f"[{form}] /data 卷内合法移动 200",
               st == 200, f"{st} {json.dumps(body, ensure_ascii=False)}")
    if not ok:
        return 1
    check(f"[{form}] 合法移动 okCount=2 / failCount=0",
          body.get("okCount") == 2 and body.get("failCount") == 0,
          json.dumps(body, ensure_ascii=False))
    st, listing = jget(f"{base}/api/library/assets?ids=" + ",".join(str(i) for i in data_ids))
    items = listing.get("items") or []
    moved_paths = [it["absPath"] for it in items]
    check(f"[{form}] DB absPath 已同步到新目录",
          len(items) == 2 and all(p.startswith(target + "/") for p in moved_paths),
          json.dumps(moved_paths, ensure_ascii=False))
    on_disk = [os.path.isfile(p) for p in moved_paths]
    check(f"[{form}] 磁盘文件真的在新目录（不是只改库）",
          len(on_disk) == 2 and all(on_disk), json.dumps(moved_paths, ensure_ascii=False))
    return 0 if FAIL == 0 else 1


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="http://127.0.0.1:8891")
    ap.add_argument("--form", default="web", choices=["web", "desktop"])
    ap.add_argument("--phase", required=True,
                    choices=["seed", "flags", "reject", "move"])
    a = ap.parse_args()
    fn = {"seed": phase_seed,
          "flags": lambda: phase_flags(a.base, a.form),
          "reject": lambda: phase_reject(a.base, a.form),
          "move": lambda: phase_move(a.base, a.form)}[a.phase]
    rc = fn()
    print(f"---- phase={a.phase} form={a.form} PASS={PASS} FAIL={FAIL}")
    return rc


if __name__ == "__main__":
    sys.exit(main())
