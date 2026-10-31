#!/usr/bin/env bash
# Python SDK 自检：单测 → conformance 12 条 → 样例包 venv 隔离 → 10 张一致性对账。
#
# 用法：
#   bash tools/pack-sdk-python/run_tests.sh              # 全部（一致性跑回放口径）
#   bash tools/pack-sdk-python/run_tests.sh --fast       # 跳过 venv 段（无 pip/无网环境）
#   bash tools/pack-sdk-python/run_tests.sh --live-vl     # 一致性追加真 VL（需 env DEEPSEEK_API_KEY）
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
PACK="$HERE/examples/ecommerce_py"
PY="${PYTHON:-python3}"
NO_VENV=0
LIVE=0
for arg in "$@"; do
  case "$arg" in
    --fast) NO_VENV=1 ;;
    --live-vl) LIVE=1 ;;
    *) echo "unknown arg: $arg"; exit 2 ;;
  esac
done

fail=0
echo "== ① 单测（SDK + 样例包规则） =="
( cd "$HERE" && "$PY" -W ignore -m unittest discover -s tests -p 'test_*.py' 2>&1 | tail -4 ) || fail=1

echo
echo "== ② conformance 12 条（假网关 + 真子进程） =="
out="$("$PY" "$ROOT/tools/pack-conformance/run.py" --plugin "$PACK" 2>&1)"
code=$?
printf '%s\n' "$out" | tail -15
[ "$code" -eq 0 ] || fail=1

if [ "$NO_VENV" -eq 0 ]; then
  echo
  echo "== ③ 每包 venv 隔离（协议 §4.0 / 2026-10-20 裁决：每包独立 venv） =="
  DAMPACK_SDK_DIR="$HERE" "$PY" - "$PACK" <<'PYEOF' || fail=1
import json, os, subprocess, sys, tempfile
pack = sys.argv[1]
sdk = os.path.dirname(os.path.dirname(os.path.abspath(pack)))   # tools/pack-sdk-python
sys.path.insert(0, sdk)
from dampack.venv import ensure_venv, probe_isolation, venv_python

work = tempfile.mkdtemp(prefix="sdk-venv-")
target_pack = os.path.join(work, "ecommerce_py")
import shutil
shutil.copytree(pack, target_pack)                   # 拷一份，别把 venv 落进仓
venv_dir = ensure_venv(target_pack, python=sys.executable,
                       pip_cache_dir=os.path.join(work, "pip-cache"))
python = venv_python(venv_dir)
print("venv 解释器：%s" % python)
print("venv 内第三方库：%s" % (probe_isolation(venv_dir) or "无（干净隔离）"))
ok = subprocess.run([python, "-c", "import dampack, dampack_launcher; "
                     "print('SDK %s @ %s' % (dampack.__version__, dampack.__file__))"],
                    capture_output=True, text=True)
print((ok.stdout or ok.stderr).strip())
if ok.returncode != 0:
    print("FAIL: venv 里装不到 SDK")
    sys.exit(1)
manifest = json.load(open(os.path.join(target_pack, "manifest.json"), encoding="utf-8"))
proc = subprocess.run([python, "-m", "dampack_launcher", target_pack], cwd=target_pack,
                      input=json.dumps({"jsonrpc": "2.0", "id": 1, "method": "pack.info",
                                        "params": {}}) + "\n",
                      capture_output=True, text=True, timeout=60,
                      env=dict(os.environ, PYTHONUNBUFFERED="1"))
try:
    info = json.loads(proc.stdout.splitlines()[0])["result"]
except (IndexError, ValueError):
    print("FAIL: venv 里 launcher 起不来\nstdout=%r\nstderr=%s"
          % (proc.stdout[:200], proc.stderr[-800:]))
    sys.exit(1)
print("launcher 起包：id=%s rules=%d caps=%d"
      % (info["id"], len(info["rules"]), len(info["capabilities"])))
print("卸载 = rm -rf %s（不写系统 site-packages）" % venv_dir)
shutil.rmtree(work, ignore_errors=True)
sys.exit(0)
PYEOF
fi

echo
echo "== ④ 10 张一致性对账（K00 基线回放） =="
"$PY" "$HERE/scripts/consistency_10.py" 2>&1 | tail -3 || fail=1

if [ "$LIVE" -eq 1 ]; then
  echo
  echo "== ⑤ 10 张一致性（VL 真调 deepseek-flash，非门禁） =="
  if [ -z "${DEEPSEEK_API_KEY:-}" ]; then
    echo "SKIP：未设 DEEPSEEK_API_KEY"
  else
    "$PY" "$HERE/scripts/consistency_10.py" --vl-live 2>&1 | tail -3 || true
  fi
fi

if [ "$fail" -eq 0 ]; then
  echo
  echo "全部通过"
else
  echo
  echo "有失败项（见上）"
fi
exit "$fail"
