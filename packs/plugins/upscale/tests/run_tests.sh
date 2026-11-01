#!/usr/bin/env bash
# 官方超分包自检：单元测试 → conformance 12 条 → 包内 venv 隔离验证。
#
# 用法：
#   bash packs/plugins/upscale/tests/run_tests.sh            # 全部
#   bash packs/plugins/upscale/tests/run_tests.sh --no-venv   # 跳过 venv 段（CI 无网/慢盘）
#   bash packs/plugins/upscale/tests/run_tests.sh --e2e       # 追加真网关语料 e2e（需模型 + 语料）
set -u
# 单测段是管道（`| tail -5`），没有 pipefail 时 `$?` 永远是 tail 的 0 ——
# 插件单测红了这个脚本也会报「全部通过」。门禁脚本必须让失败浮出来（K07-FIX-G）。
set -o pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
PACK="$(cd "$HERE/.." && pwd)"
ROOT="$(cd "$PACK/../../.." && pwd)"
PY="${PYTHON:-python3}"
NO_VENV=0
E2E=0
for arg in "$@"; do
  case "$arg" in
    --no-venv) NO_VENV=1 ;;
    --e2e) E2E=1 ;;
    *) echo "unknown arg: $arg"; exit 2 ;;
  esac
done

fail=0
echo "== ① 单元测试（$(basename "$PY")） =="
( cd "$HERE" && "$PY" -m unittest discover -s . -p 'test_*.py' 2>&1 | tail -5 ) || fail=1

echo
echo "== ② conformance 12 条（假网关 + 真子进程） =="
"$PY" "$ROOT/tools/pack-conformance/run.py" --plugin "$PACK" 2>&1 | tail -16 || fail=1

if [ "$NO_VENV" -eq 0 ]; then
  echo
  echo "== ③ 隔离解释器跑一遍（协议 §4.0：零第三方依赖） =="
  VENV="$(mktemp -d)/venv"
  if "$PY" -m venv "$VENV" >/dev/null 2>&1; then
    "$VENV/bin/python" -m pip install --quiet --disable-pip-version-check \
        -r "$PACK/requirements.txt" || { echo "FAIL: pip install -r 失败"; fail=1; }
    # 用 **venv 解释器**当套件宿主：插件子进程随之用 venv python，
    # 证明它不偷宿主 site-packages
    "$VENV/bin/python" "$ROOT/tools/pack-conformance/run.py" --plugin "$PACK" 2>&1 | tail -3 \
        || fail=1
    rm -rf "$(dirname "$VENV")"
  else
    # 本机没 ensurepip（dam-dev 容器就是）→ 退到 `-I`（隔离模式：不读 site-packages/
    # 用户目录/PYTHONPATH），同样是「只用标准库才能起」的证据，不假装跑过 venv
    echo "（本机无 ensurepip，venv 段退化为 python3 -I 隔离模式）"
    "$PY" -I "$ROOT/tools/pack-conformance/run.py" --plugin "$PACK" 2>&1 | tail -3 || fail=1
  fi
fi

if [ "$E2E" -eq 1 ]; then
  echo
  echo "== ④ 真网关 + 真模型 + 真语料 e2e（10 张尺寸/PSNR + 幂等 + 分块 + 并发） =="
  ( cd "$ROOT" && DAM_GATEWAY_E2E_MODELS="${DAM_GATEWAY_E2E_MODELS:-/models}" \
      DAMDESK_CORPUS="${DAMDESK_CORPUS:-/tmp/corpus_v1}" \
      cargo test -q -p pack-host --test upscale_pack_e2e -- --nocapture --test-threads=1 2>&1 \
      | tail -20 ) || fail=1
fi

if [ "$fail" -eq 0 ]; then
  echo
  echo "全部通过"
else
  echo
  echo "有失败项（见上）"
fi
exit "$fail"
