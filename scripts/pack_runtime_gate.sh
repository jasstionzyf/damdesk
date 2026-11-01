#!/usr/bin/env bash
# 出包门禁（KI-28，卡 t_5c9accc5）：安装包里**必须**带内置 python 运行时 + 随包启动器。
#
# 为什么是硬门禁：上一版（K14）包里 `grep -i python` 命中 0 条，纯净 Windows 上行业包
# （合规判定插件）一律起不来，而构建全程 zero warning —— 这种「装好了但功能永远不可用」
# 只能靠打包时断死。tauri `beforeBundleCommand` 挂的就是本脚本。
#
# 用法：bash scripts/pack_runtime_gate.sh      # 退出码非 0 = 不许出包
set -uo pipefail
# 平台门禁：内置 python runtime 是 win-x64 专属（python.exe / python311.dll，见 stage_pack_runtime.py），
# macOS/Linux 包不该有它、也不该被它判红 → 非 Windows 宿主直接跳过，别让 Mac/Linux 构建假红。
case "$(uname -s 2>/dev/null || echo unknown)" in
  MINGW*|MSYS*|CYGWIN*|Windows_NT) ;;
  *) echo "SKIP pack_runtime_gate：宿主 $(uname -s 2>/dev/null) 非 Windows，内置 runtime 门禁不适用" >&2; exit 0 ;;
esac
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RES="$ROOT/crates/app-tauri/resources"
PY="$RES/runtime/python/python.exe"
LAUNCHER="$RES/pack-launcher/dampack_launcher.py"
SRC_LAUNCHER="$ROOT/tools/dampack_launcher.py"
fail=0

if [ ! -f "$PY" ]; then
  echo "FAIL 安装包里没有内置 python 运行时：$PY" >&2
  echo "     先跑：python3 scripts/stage_pack_runtime.py" >&2
  fail=1
fi
for need in "Lib/venv/__init__.py" "Lib/ensurepip/__init__.py" "python311.dll"; do
  [ -f "$RES/runtime/python/$need" ] || { echo "FAIL runtime 缺 ${need}（venv/每包隔离会不可用）" >&2; fail=1; }
done
if [ ! -f "$LAUNCHER" ]; then
  echo "FAIL 安装包里没有随包启动器：${LAUNCHER}（先跑 scripts/stage_pack_runtime.py）" >&2
  fail=1
elif ! cmp -s "$LAUNCHER" "$SRC_LAUNCHER"; then
  echo "FAIL 随包启动器与 tools/dampack_launcher.py 不同步（重跑 stage，别让包里的和仓库里的分叉）" >&2
  fail=1
fi
if [ "$fail" = 0 ]; then
  echo "PASS 内置 runtime 门禁：$PY ($(du -sh "$RES/runtime/python" | cut -f1) 未压缩) + 随包启动器"
fi
exit "$fail"
