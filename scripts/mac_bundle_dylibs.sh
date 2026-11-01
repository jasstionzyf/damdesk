#!/usr/bin/env bash
# mac_bundle_dylibs.sh —— 把 .app 对 /opt/homebrew 动态库的依赖闭包收进
# Contents/Frameworks，产出**免 brew 的自包含 .app**（D13 坑#4 的落地）。
#
# 做法（mac app 分发标准套路）：
#   1. 从主二进制开始 BFS 收集所有 /opt/homebrew/*.dylib 依赖闭包；
#   2. 闭包拷进 Contents/Frameworks/，每个 dylib 的 install id 改 @rpath/<name>，
#      引用方（主二进制 + 每个 dylib）的 LC_LOAD_DYLIB 全部 -change 成 @rpath/<name>；
#   3. 主二进制 -add_rpath @executable_path/../Frameworks；
#   4. libvips 的动态格式模块（heif/jxl/magick/poppler/openslide）整个目录拷进
#      Frameworks/vips-modules-<ver>/，模块自身依赖同样收闭包改 @rpath；
#      运行时由 core-media 用 VIPS_LIBDIR 指过去（vips.rs point_vips_libdir_at_bundle）；
#   5. 改过 load commands 的 Mach-O 在 arm64 上必须重签（ad-hoc），否则 dyld 直接 kill；
#   6. 收尾自检：递归 otool -L 不允许再出现 /opt/homebrew（退出码非 0 = 失败）。
#
# 用法：bash scripts/mac_bundle_dylibs.sh "target/release/bundle/macos/souJpg DAM.app"
# 时机：pnpm tauri build --bundles app 之后、打 dmg 之前（reports/build_dmg_mac.sh 两阶段）。
#
# 已知边界：fontconfig 运行时仍读系统/brew 的 fonts.conf（只影响 vips 文本渲染，
# DAM 不用）；除 /opt/homebrew 外的系统库（/usr/lib）不收集、不需要。
set -euo pipefail
APP="${1:?用法: mac_bundle_dylibs.sh <path/to/App.app>}"
BIN="$APP/Contents/MacOS/app-tauri"
FW="$APP/Contents/Frameworks"
[ -f "$BIN" ] || { echo "FAIL 找不到主二进制：$BIN" >&2; exit 1; }
mkdir -p "$FW"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
Q="$TMP/queue"; DONEF="$TMP/done"
: > "$Q"; : > "$DONEF"

# 只收 /opt/homebrew 的动态库依赖（系统库不动）
deps_of() { otool -L "$1" | awk '/^\t\/opt\/homebrew\//{print $1}' | sort -u; }

# 把一个 Mach-O 对 homebrew 库的引用全部改成 @rpath/<basename>
fix_refs() {
  local f="$1" dep name
  while IFS= read -r dep; do
    [ -n "$dep" ] || continue
    name="$(basename "$dep")"
    install_name_tool -change "$dep" "@rpath/$name" "$f"
  done < <(deps_of "$f")
}

pop_queue() { src_="$(head -1 "$Q")"; tail -n +2 "$Q" > "$Q.tmp" && mv "$Q.tmp" "$Q"; }

# 伴生库收集：openexr/imath 等对同目录兄弟库用 @rpath/<name> / @loader_path/<name>
# 引用（otool 里不是 /opt/homebrew 绝对路径，闭包会漏）。按源 dylib 所在目录解析。
sibling_deps() { # $1=刚拷进 FW 的 Mach-O，$2=源目录
  local f="$1" dir="$2" ref base
  while IFS= read -r ref; do
    [ -n "$ref" ] || continue
    base="${ref##*/}"
    [ -f "$FW/$base" ] && continue
    [ -f "$dir/$base" ] && echo "$dir/$base" >> "$Q"
  done < <(otool -L "$f" | awk '/^\t@(rpath|loader_path)\//{print $1}')
}

echo "== 收集依赖闭包（主二进制 → Frameworks）"
# 注意：同一 dylib 在 otool 里可能以 /opt/homebrew/opt/<f>/lib/ 和 /opt/homebrew/lib/
# 两种路径出现（brew 的 opt 都是 ../lib 的符号链接）；去重按 basename，
# 且 brew 源文件只读（r--r--r--），覆盖已存在的只读目标前先 chmod。
copy_in() {
  local src_="$1" name_ dst_
  name_="$(basename "$src_")"
  dst_="$FW/$name_"
  if [ -f "$dst_" ] && cmp -s "$src_" "$dst_"; then return 0; fi
  [ -f "$dst_" ] && chmod u+w "$dst_"
  cp -f "$src_" "$dst_"
}
deps_of "$BIN" >> "$Q"
while [ -s "$Q" ]; do
  pop_queue
  name="$(basename "$src_")"
  grep -qx "$name" "$DONEF" && continue
  echo "$name" >> "$DONEF"
  # 注意：必须先收依赖再改引用（改完 otool 就看不到 /opt/homebrew 了）
  copy_in "$src_"
  deps_of "$FW/$name" >> "$Q"
  sibling_deps "$FW/$name" "$(dirname "$src_")"
  install_name_tool -id "@rpath/$name" "$FW/$name"
  fix_refs "$FW/$name"
done
fix_refs "$BIN"

echo "== rpath"
if ! otool -l "$BIN" | grep -q 'path @executable_path/../Frameworks'; then
  install_name_tool -add_rpath '@executable_path/../Frameworks' "$BIN"
fi

echo "== libvips 动态格式模块"
# 模块放 Contents/lib/vips-modules-<ver>（不进 Frameworks：codesign 会把 Frameworks 下
# 的普通目录当 bundle subcomponent 拒签）。运行时 core-media 把 VIPSHOME 指到
# .app/Contents（实测 libvips 8.18.7：VIPSHOME 完全接管 prefix 猜测，找
# $VIPSHOME/lib/vips-modules-<ver>；VIPS_LIBDIR/VIPS_PREFIX 均无效）。
# 模块对库的引用改 @executable_path/../Frameworks/<name>（dlopen 上下文里 @rpath
# 不一定沿主程序传播，@executable_path 恒指向主程序，最稳）
VLIB="$APP/Contents/lib"
mod_fix_refs() {
  local f="$1" dep name
  while IFS= read -r dep; do
    [ -n "$dep" ] || continue
    name="$(basename "$dep")"
    install_name_tool -change "$dep" "@executable_path/../Frameworks/$name" "$f"
  done < <(deps_of "$f")
}
for MODDIR in /opt/homebrew/lib/vips-modules-*; do
  [ -d "$MODDIR" ] || continue
  ver="$(basename "$MODDIR")"
  mkdir -p "$VLIB/$ver"
  # 历史遗留防御：旧版曾把模块放 Resources（VIPSHOME 方案下无效），清掉
  rm -rf "$APP/Contents/Resources/$ver" 2>/dev/null || true
  for mod in "$MODDIR"/*.dylib; do
    name="$(basename "$mod")"
    if [ ! -f "$VLIB/$ver/$name" ]; then
      cp "$mod" "$VLIB/$ver/$name"
    elif ! cmp -s "$mod" "$VLIB/$ver/$name"; then
      chmod u+w "$VLIB/$ver/$name"; cp -f "$mod" "$VLIB/$ver/$name"
    fi
    deps_of "$VLIB/$ver/$name" >> "$Q"
    mod_fix_refs "$VLIB/$ver/$name"
  done
  echo "   $ver: $(ls "$VLIB/$ver" | wc -l | tr -d ' ') 个模块 → Contents/lib/$ver"
done
# 模块引出的新依赖闭包（libheif 等）
while [ -s "$Q" ]; do
  pop_queue
  name="$(basename "$src_")"
  grep -qx "$name" "$DONEF" && continue
  echo "$name" >> "$DONEF"
  copy_in "$src_"
  deps_of "$FW/$name" >> "$Q"
  sibling_deps "$FW/$name" "$(dirname "$src_")"
  install_name_tool -id "@rpath/$name" "$FW/$name"
  fix_refs "$FW/$name"
done

echo "== ad-hoc 重签（arm64 改 load commands 后必须）"
# 历史遗留防御：旧版脚本曾把模块放进 Frameworks（会被 codesign 拒签），清掉
rm -rf "$FW"/vips-modules-* 2>/dev/null || true
codesign --force --sign - --timestamp=none "$FW"/*.dylib 2>/dev/null
for MODDIR in "$VLIB"/vips-modules-*; do
  [ -d "$MODDIR" ] && codesign --force --sign - --timestamp=none "$MODDIR"/*.dylib 2>/dev/null
done
# 主二进制不能用裸文件签（codesign 会把 .app 内目录当 subcomponent 校验），
# 对整个 .app --deep 签（覆盖 Frameworks 里所有 Mach-O + 主二进制 + 资源清单）
codesign --force --sign - --timestamp=none --deep "$APP"

echo "== 自检：递归 otool 不得再出现 /opt/homebrew"
bad=0
check_one() {
  local n; n="$(deps_of "$1" | wc -l | tr -d ' ')"
  if [ "$n" != "0" ]; then echo "FAIL $1 仍引用 homebrew：" >&2; deps_of "$1" >&2; bad=1; fi
}
# 伴生库检查：@rpath/@loader_path 引用的 basename 必须已在 FW（否则 dyld 运行时崩，
# 纯 homebrew 检查查不出来 —— libIlmThread 教训）
sibling_check() {
  local ref base
  while IFS= read -r ref; do
    [ -n "$ref" ] || continue
    base="${ref##*/}"
    if [ ! -f "$FW/$base" ]; then echo "FAIL $1 的 $ref 在 Frameworks 找不到同名伴生库" >&2; bad=1; fi
  done < <(otool -L "$1" | awk '/^\t@(rpath|loader_path)\//{print $1}')
}
check_one "$BIN"; sibling_check "$BIN"
for f in "$FW"/*.dylib "$VLIB"/vips-modules-*/*.dylib; do [ -f "$f" ] && { check_one "$f"; sibling_check "$f"; }; done
[ "$bad" = 0 ] || exit 1

echo "PASS 自包含 dylib：$(ls "$FW"/*.dylib | wc -l | tr -d ' ') 个 Frameworks dylib + Contents/lib/vips-modules-*（VIPSHOME 由 core-media 运行时指向）"
