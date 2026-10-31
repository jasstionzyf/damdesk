#!/usr/bin/env bash
# 发布 damdesk 增强模型物料到 HF（主源）+ 腾讯 COS（镜像），并做字节/哈希校验。
#
# 前置：scripts/build_models_pkg.py --check-only 必须先通过（防「下载下来 ready=false」
# 的唯一保障）；凭据不落本脚本：HF_TOKEN 走环境变量，COS 走 ~/.cos.conf。
#
# 双形态分发树（见 build_models_pkg.py 模块注释）：
#   zip 树 = <name>/<file>（落盘布局，zip 解压到 models root 即 ready）
#   HTTP 树 = url_path 形态（base_url + url_path；u2net/realesr/lama 是顶层单文件）
# COS/HF 两种形态都要有；本脚本用同桶 copy 补顶层对象（零流量）。
#
# 用法：
#   scripts/publish_models.sh                  # 全流程：校验 → HF（无 token 则打印命令）→ COS → 校验
#   scripts/publish_models.sh --verify-cos-only # 只做 COS 侧校验（HEAD + 抽样 sha256）
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PKG="${PKG:-/data}"
ZIP="${ZIP:-/data}"
MANIFEST="${MANIFEST:-$PKG/models-manifest.txt}"
HF_REPO="${HF_REPO:-damdesk/models}"
COS_BUCKET="${COS_BUCKET:-soujpg-images-1307121509}"   # 备选新建 damdesk-models-1307121509（见交付说明）
COS_REGION="${COS_REGION:-ap-shanghai}"
COS_PREFIX="${COS_PREFIX:-damdesk/models}"
COS_HTTP="https://${COS_BUCKET}.cos.${COS_REGION}.myqcloud.com"
WORK="$(mktemp -d /tmp/damdesk-publish.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
log()  { echo "== $*"; }

# ---------- 前置：包与代码一致性 ----------
log "前置校验：pkg ↔ model_store.rs"
python3 "$REPO/scripts/build_models_pkg.py" --check-only

# ---------- HF 主源 ----------
log "HF（$HF_REPO）"
HF_CMDS=()
HF_CMDS+=("HF_TOKEN=<token> huggingface-cli upload $HF_REPO $PKG --repo-type model")
HF_CMDS+=("HF_TOKEN=<token> huggingface-cli upload $HF_REPO $ZIP --repo-type model")
# HTTP 树形态（base_url=HF 仓根时需要）：把 url_path 指向 pkg 内文件
while IFS=$'\t' read -r url_path pkg_rel _bytes; do
  [[ "$url_path" == "$pkg_rel" ]] && continue  # 同形已在目录上传覆盖
  HF_CMDS+=("HF_TOKEN=<token> huggingface-cli upload $HF_REPO $PKG/$pkg_rel $url_path --repo-type model")
done < <(python3 "$REPO/scripts/build_models_pkg.py" --emit-http-tree)

if [[ -n "${HF_TOKEN:-}" ]]; then
  for c in "${HF_CMDS[@]}"; do
    log "HF: ${c/HF_TOKEN=<token> /}"
    eval "${c/HF_TOKEN=<token> /HF_TOKEN=$HF_TOKEN }"
  done
  log "HF 上传完成"
else
  echo "SKIP: HF_TOKEN 未设置 —— HF 未上传。待执行命令："
  printf '  %s\n' "${HF_CMDS[@]}"
fi

# ---------- COS 镜像 ----------
verify_size() {  # $1=cos_path $2=expected_bytes；匿名优先，403 降级 signurl。输出 0/1
  local cos_path="$1" expected="$2" actual="" code
  code=$(curl -sI -o /dev/null -w '%{http_code}' "$COS_HTTP/$cos_path" || true)
  if [[ "$code" == "200" ]]; then
    actual=$(curl -sI "$COS_HTTP/$cos_path" | awk 'tolower($1)=="content-length:"{print $2}' | tr -d '\r')
    [[ "$actual" == "$expected" ]]
    return
  fi
  local surl; surl=$(coscmd signurl -t 3600 "$cos_path" </dev/null 2>/dev/null | tail -1 | sed "s/^b'//; s/'\$//")
  actual=$(curl -s -r 0-0 -D - -o /dev/null "$surl" \
           | awk 'tolower($1)=="content-range:"{split($3,a,"/");print a[2]}' | tr -d '\r')
  if [[ -z "$actual" ]]; then
    echo "  ✗ $cos_path: 匿名=$code 签名取长失败" >&2; return 1
  fi
  [[ "$actual" == "$expected" ]]
}

if [[ "${1:-}" == "--verify-cos-only" ]]; then
  log "COS 校验（verify-only）"
else
  log "COS 上传：zip 树 + manifest + HTTP 树补齐"
  command -v coscmd >/dev/null || fail "coscmd 不可用（需 ~/.cos.conf）"
  coscmd -b "$COS_BUCKET" -r "$COS_REGION" upload -f "$MANIFEST" "$COS_PREFIX/models-manifest.txt"
  coscmd -b "$COS_BUCKET" -r "$COS_REGION" upload -f "$ZIP" "$COS_PREFIX/$(basename "$ZIP")"
  # zip 树（目录增量，-s 跳过相同）
  coscmd -b "$COS_BUCKET" -r "$COS_REGION" upload -rs "$PKG/" "$COS_PREFIX/" -y </dev/null
  # HTTP 树形态：同桶服务端 copy（零流量），只补与 zip 树不同形的 url_path
  while IFS=$'\t' read -r url_path pkg_rel _bytes; do
    [[ "$url_path" == "$pkg_rel" ]] && continue
    coscmd -b "$COS_BUCKET" -r "$COS_REGION" copy \
      "$COS_BUCKET.cos.${COS_REGION}.myqcloud.com/$COS_PREFIX/$pkg_rel" "$COS_PREFIX/$url_path" -y </dev/null
    echo "  HTTP 树补齐：$COS_PREFIX/$url_path (copy ← $pkg_rel)"
  done < <(python3 "$REPO/scripts/build_models_pkg.py" --emit-http-tree)
fi

log "COS 校验：coscmd list + 逐对象字节比对"
coscmd -b "$COS_BUCKET" -r "$COS_REGION" list "$COS_PREFIX/" | tee "$WORK/cos.list"

FAILS=0
# zip 树对象（含 manifest 与 zip 本体）
while IFS=$'\t' read -r model fname bytes _sha _lic _src; do
  [[ "$model" == \#* || -z "${bytes:-}" ]] && continue
  cos_path="$COS_PREFIX/$model/$fname"
  if verify_size "$cos_path" "$bytes"; then
    echo "  ✓ $cos_path = $bytes B"
  else
    echo "  ✗ $cos_path 字节不符"; FAILS=$((FAILS+1))
  fi
done < "$MANIFEST"
for f in "models-manifest.txt:$(stat -c%s "$MANIFEST")" "$(basename "$ZIP"):$(stat -c%s "$ZIP")"; do
  cos_path="$COS_PREFIX/${f%%:*}"; bytes="${f##*:}"
  if verify_size "$cos_path" "$bytes"; then
    echo "  ✓ $cos_path = $bytes B"
  else
    echo "  ✗ $cos_path 字节不符"; FAILS=$((FAILS+1))
  fi
done
# HTTP 树对象
while IFS=$'\t' read -r url_path pkg_rel bytes; do
  cos_path="$COS_PREFIX/$url_path"
  if verify_size "$cos_path" "$bytes"; then
    echo "  ✓ HTTP $cos_path = $bytes B"
  else
    echo "  ✗ HTTP $cos_path 字节不符"; FAILS=$((FAILS+1))
  fi
done < <(python3 "$REPO/scripts/build_models_pkg.py" --emit-http-tree)

log "COS 抽样：随机 1 个模型文件全量下载比对 sha256"
SAMPLE_LINE=$(grep -v '^#' "$MANIFEST" | shuf -n1)
IFS=$'\t' read -r smodel sfile sbytes ssha _rest <<<"$SAMPLE_LINE"
surl=$(coscmd signurl -t 7200 "$COS_PREFIX/$smodel/$sfile" 2>/dev/null | tail -1 | sed "s/^b'//; s/'\$//")
curl -sL -o "$WORK/sample" "$surl"
actual_sha=$(sha256sum "$WORK/sample" | awk '{print $1}')
if [[ "$actual_sha" == "$ssha" ]]; then
  echo "  ✓ $smodel/$sfile 全量 sha256 一致（$(stat -c%s "$WORK/sample") B）"
else
  echo "  ✗ $smodel/$sfile sha256 不符：$actual_sha ≠ $ssha"; FAILS=$((FAILS+1))
fi

[[ "$FAILS" -eq 0 ]] || fail "COS 校验 $FAILS 项不符"
echo "OK: 双源发布与校验完成（HF 见上方输出；COS=$COS_HTTP/$COS_PREFIX/）"
