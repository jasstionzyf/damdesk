#!/usr/bin/env bash
# 一条命令验证 conformance 套件自身（不是验证某个包）：
#   · fixtures/good            → 必须 12/12 且退出码 0
#   · 四个违规变体             → 必须退出码非 0，且**失败的断言编号恰好等于预期集合**
# 只验「有失败」不够 —— 断言必须打在它该打的地方，否则套件等于空转。
#
# 用法：bash tools/pack-conformance/run_all.sh
set -u

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT" || exit 2

# i18n-F：对象形态端到端夹具 —— 从 fixtures/good 临时派生一份「名字/短名/描述全用
# {"zh-Hans":…,"en-US":…} 对象」的包，必须同样 14/14 全绿（老包字符串形态由原 good 覆盖）。
GOOD_I18N="$(mktemp -d)/good-i18n"
mkdir -p "$GOOD_I18N"
cp -r tools/pack-conformance/fixtures/good/. "$GOOD_I18N/"
python3 - "$GOOD_I18N/manifest.json" <<'PYEOF'
import json, sys, collections
path = sys.argv[1]
m = json.load(open(path, encoding="utf-8"), object_pairs_hook=collections.OrderedDict)
m["name"] = collections.OrderedDict([("zh-Hans", "参考实现包"), ("en-US", "Reference Pack")])
m["short_name"] = collections.OrderedDict([("zh-Hans", "参考包"), ("en-US", "Ref Pack")])
m["summary"] = collections.OrderedDict([("zh-Hans", "conformance 参考实现（对象形态名）。"),
                                        ("en-US", "Conformance reference pack (object-form names).")])
json.dump(m, open(path, "w", encoding="utf-8"), ensure_ascii=False, indent=2)
PYEOF

PACKS=(
  "fixtures/good|"
  "$GOOD_I18N|"
  "fixtures/bad-stdout-pollution|02"
  "fixtures/bad-missing-method|05,10,12"
  "fixtures/bad-error-code-misuse|08,09"
  "fixtures/bad-no-health|04"
)

normalize() {
  printf '%s' "$1" | tr ',' '\n' | grep -v '^[[:space:]]*$' | sort | tr '\n' ',' | sed 's/,$//'
}

overall=0
for entry in "${PACKS[@]}"; do
  pack="${entry%%|*}"
  expect="$(normalize "${entry##*|}")"

  # i18n-F 夹具给的是绝对路径（mktemp），仓库内夹具给相对路径 —— 两者都归一成绝对
  case "$pack" in
    /*) plugin_dir="$pack" ;;
    *)  plugin_dir="$ROOT/tools/pack-conformance/$pack" ;;
  esac

  out="$(python3 tools/pack-conformance/run.py --plugin "$plugin_dir" 2>&1)"
  code=$?
  reported="$(printf '%s\n' "$out" | sed -n 's/^FAILED: //p' | tr -d ' ' | tr ',' '\n' | grep -v '^$' | sort | tr '\n' ',' | sed 's/,$//')"
  summary="$(printf '%s\n' "$out" | grep -E '^[0-9]+/[0-9]+ PASS' || true)"

  if [ -z "$expect" ]; then
    if [ "$code" -eq 0 ] && [ -z "$reported" ]; then
      echo "OK   $(basename "$pack")            ${summary}"
    else
      echo "BAD  $(basename "$pack")            exit=$code ${summary} failed=$reported"
      overall=1
    fi
  else
    if [ "$code" -ne 0 ] && [ "$reported" = "$expect" ]; then
      echo "OK   $pack            ${summary} failed=$reported"
    else
      echo "BAD  $pack            exit=$code ${summary} 期望=$expect 实得=$reported"
      overall=1
    fi
  fi
done

if [ "$overall" -eq 0 ]; then
  echo "conformance 套件自检：全部符合预期"
else
  echo "conformance 套件自检：有偏差（见上）"
fi
exit "$overall"
