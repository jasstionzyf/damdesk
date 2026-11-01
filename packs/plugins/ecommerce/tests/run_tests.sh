#!/usr/bin/env bash
# 官方电商包自检：单元测试 → conformance 12 条 → 包内 venv 隔离验证。
#
# 用法：
#   bash packs/plugins/ecommerce/tests/run_tests.sh            # 全部
#   bash packs/plugins/ecommerce/tests/run_tests.sh --no-venv   # 跳过 venv 段（CI 无网/慢盘）
set -u
# 单测段是管道（`| tail -5`），没有 pipefail 时 `$?` 永远是 tail 的 0 ——
# 插件单测红了这个脚本也会报「全部通过」。门禁脚本必须让失败浮出来（K07-FIX-G）。
set -o pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
PACK="$(cd "$HERE/.." && pwd)"
ROOT="$(cd "$PACK/../../.." && pwd)"
PY="${PYTHON:-python3}"
NO_VENV=0
[ "${1:-}" = "--no-venv" ] && NO_VENV=1

fail=0
echo "== ① 单元测试（$(basename "$PY")） =="
( cd "$HERE" && "$PY" -m unittest discover -s . -p 'test_*.py' -v 2>&1 | tail -5 ) || fail=1

echo
echo "== ② conformance 12 条（假网关 + 真子进程） =="
"$PY" "$ROOT/tools/pack-conformance/run.py" --plugin "$PACK" 2>&1 | tail -16 || fail=1

if [ "$NO_VENV" -eq 0 ]; then
  echo
  echo "== ③ 包内 venv 隔离（协议 §4.0：每包独立 venv，零第三方依赖） =="
  VENV="$(mktemp -d)/venv"
  "$PY" -m venv "$VENV" || { echo "FAIL: 建 venv 失败"; fail=1; }
  "$VENV/bin/python" -m pip install --quiet --disable-pip-version-check \
      -r "$PACK/requirements.txt" || { echo "FAIL: pip install -r 失败"; fail=1; }
  # 用 **venv 解释器**当套件宿主：插件子进程随之用 venv python，
  # 证明它不偷宿主 site-packages（零第三方依赖）
  "$VENV/bin/python" "$ROOT/tools/pack-conformance/run.py" --plugin "$PACK" 2>&1 | tail -3 \
      || fail=1
  "$VENV/bin/python" - "$PACK" <<'PYEOF' || fail=1
import importlib.util, json, os, subprocess, sys
pack = sys.argv[1]
manifest = json.load(open(os.path.join(pack, "manifest.json"), encoding="utf-8"))
env = dict(os.environ, DAMPACK_LOG_LEVEL="info")
proc = subprocess.run([sys.executable] + manifest["entrypoint"]["args"], cwd=pack,
                      input=json.dumps({"jsonrpc": "2.0", "id": 1,
                                        "method": "pack.info", "params": {}}) + "\n",
                      capture_output=True, text=True, env=env, timeout=30)
info = json.loads(proc.stdout.splitlines()[0])["result"]
third_party = [m for m in ("numpy", "PIL", "cv2", "requests")
               if importlib.util.find_spec(m) is not None]
print("pack.info id=%s（venv 内第三方库：%s）" % (info["id"], third_party or "无"))
sys.exit(0 if info["id"] else 1)
PYEOF
  echo "venv 解释器：$("$VENV/bin/python" -c 'import sys;print(sys.executable)')"
  echo "卸载 = rm -rf $(dirname "$VENV")（不写系统 site-packages，删目录即净）"
  rm -rf "$(dirname "$VENV")"
fi

if [ "$fail" -eq 0 ]; then
  echo
  echo "全部通过"
else
  echo
  echo "有失败项（见上）"
fi
exit "$fail"
