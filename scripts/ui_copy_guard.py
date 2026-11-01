#!/usr/bin/env python3
"""N12 文案守卫：用户可见文案里不许出现内部词。

设计口径（`damdesk-product/references/plugin-design-and-ui.md` §3）：

    用户视角只有两动词三名词 —— 动词「检查」「处理」，名词「图集」「规格」「结果」。
    界面禁用词：插件 / job / verdict / 行业包。

扫描范围：`web/src/**/*.tsx`、`web/src/**/*.ts` 的**文案**（JSX 文本节点 + 字符串字面量，
注释与标识符不算）。允许的例外只有「插件与方案目录」这一处设置面（`SETTINGS_SURFACE`）——
那里的用户任务就是装/卸/换版本，藏掉这个词反而说不清。

i18n 词典（i18n-G，卡 t_25f3b8c8）：`web/src/i18n/dict.*.ts` 的 **key** 是界面中文原文
（source-text-as-key），key 里出现内部词是被迁移的原文案，不算违规；但**值**（用户真正读到的
译文，含 `errors.<code>` 错误码词条）与其它文案同标准 —— 值里出现禁用词就是文案缺陷。
所以词典文件按「key 豁免、value 纳入扫描」处理。

用法：
    python3 scripts/ui_copy_guard.py            # 打印违规并退出码 1（有违规）
    python3 scripts/ui_copy_guard.py --quiet    # 只打结论行
    python3 scripts/ui_copy_guard.py --src DIR   # 换扫描根（自测用，见 scripts/ui_copy_guard_selftest.py）
退出码：0 = 0 命中；1 = 有命中；2 = 扫描目录不存在（守卫本身失效，不能当成通过）。
"""
from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "web" / "src"   # 可被 `--src` 覆盖（自测指向临时夹具）

# 禁用词（用户可见文案）
FORBIDDEN = [
    (re.compile(r"行业包"), "行业包"),
    (re.compile(r"插件"), "插件"),
    (re.compile(r"\bjob\b", re.IGNORECASE), "job"),
    (re.compile(r"\bverdict\b", re.IGNORECASE), "verdict"),
    # KI35-P4（t_7840aaac）收口：§3 命名表裁决「规格的用户面命名 = 方案」，
    # 用户面一律「方案」；「规格」降为内部词，与 插件/job/verdict 同列红线。
    (re.compile(r"规格"), "规格"),
    # manifest 同为红线词：只允许作为**真实文件名** manifest.csv / manifest.json 出现
    #（导出清单文件、包内清单 —— 改名等于改交付契约，不属于文案问题）。
    (re.compile(r"manifest(?!\.(?:csv|json))", re.IGNORECASE), "manifest"),
]

# 唯一允许出现「插件」的设置面（装/卸/换版本 = 用户任务，见设计 §4）
SETTINGS_SURFACE = {
    "components/PackManager.tsx",
    "pages/Settings.tsx",
    "App.tsx",  # Drawer 标题「插件与方案」
    # N链合流（N4 × N12）：插件管理面在 N4 被拆出详情页，但**还是同一个设置面** ——
    # 用户在这里的任务就是装 / 卸 / 换版本，藏掉这个词说不清（见设计 §4）。
    # KI35-P2（t_6f03111f）起详情页三段（能力说明/我的方案/默认设置）按 §3 用户面命名表
    # 重写为 方案/默认设置/处理记录 —— PluginDetail / ParamForm / SpecEditorDialog 的
    # 用户面文案已 0 命中「插件」，从白名单移除（留在这里会让红线失去守卫）；装/卸/换版本
    # 的管理字样仍由 PackInstaller / PackManager / lib/plugins.ts 承载。
    "components/PackInstaller.tsx",
    "lib/plugins.ts",
}

STRING_RE = re.compile(r"""'(?:[^'\\]|\\.)*'|"(?:[^"\\]|\\.)*\"""", re.S)

# i18n 词典文件（`web/src/i18n/dict.*.ts`）：key = 界面中文原文，值是译文。
# 对它们只扫**值**（见 dict_values）；key 里出现「插件 / manifest」是被迁移的原文案，
# 其来源文件另有守卫（SETTINGS_SURFACE 例外等）。
DICT_FILE_RE = re.compile(r"/i18n/dict\.[\w-]+\.ts$")
DICT_ENTRY_RE = re.compile(
    r"""^\s*(?:'(?:[^'\\]|\\.)*'|"(?:[^"\\]|\\.)*"|[\w$]+)\s*:\s*"""
    r"""('((?:[^'\\]|\\.)*)'|"((?:[^"\\]|\\.)*)"|`((?:[^`\\]|\\.)*)`)""",
    re.M,
)


def strip_comments(src: str) -> str:
    """去掉 // 与 /* */ 注释（不追求完美：跳过字符串里的 // 即可，够用且不会漏检文案）。"""
    out = []
    i, n = 0, len(src)
    in_s = None
    while i < n:
        ch = src[i]
        two = src[i : i + 2]
        if in_s:
            out.append(ch)
            if ch == "\\":
                out.append(src[i + 1 : i + 2])
                i += 2
                continue
            if ch == in_s:
                in_s = None
            i += 1
            continue
        if two == "//":
            j = src.find("\n", i)
            i = n if j < 0 else j
            continue
        if two == "/*":
            j = src.find("*/", i + 2)
            i = n if j < 0 else j + 2
            continue
        if ch in "\"'`":
            in_s = ch
        out.append(ch)
        i += 1
    return "".join(out)


def jsx_text_nodes(src: str) -> list[str]:
    """JSX 文本节点：`>` 与 `<` 之间、不含 `{}` 的连续片段（`{}` 里是代码表达式）。

    i18n-G（卡 t_25f3b8c8）重写：旧实现用「`{` 进入表达式、配对 `}` 退出」的状态机，
    而组件体本身就在一对 `{}` 里 ⇒ 函数体里的 JSX 全被当成代码跳过，**JSX 文案实际从未被扫**。
    实证：`export function D(){return <p>行业包未安装</p>}` 旧实现 0 命中（自测反例抓出来的）。
    现在改为在（已去注释的）源码上直接取 `>…<` 片段，额外要求片段里含中文 —— 源码里的用户文案
    是中文原文（i18n 后仍是，key 就是它），这样比较运算 `a > b < c` 之类的假片段不会命中。
    """
    out: list[str] = []
    cjk = re.compile(r"[\u4e00-\u9fff]")
    for m in re.finditer(r">([^<>{}]+)<", src):
        text = " ".join(m.group(1).split())
        if text and cjk.search(text):
            out.append(text)
    return out


def strings(src: str) -> list[str]:
    out = []
    for m in STRING_RE.finditer(src):
        out.append(m.group(0))
    # 模板字面量：只取静态片段（${...} 里是代码）
    for m in re.finditer(r"`(?:[^`\\]|\\.)*`", src, re.S):
        out.append(re.sub(r"\$\{[^}]*\}", " ", m.group(0)))
    return out


def dict_values(src: str) -> list[str]:
    """i18n 词典文件的**值**（用户真正看到的译文）。

    key 是界面中文原文（source-text-as-key），它内部可以出现「插件」「manifest」——
    那是被迁移的原文案，另有来源文件守卫（`插件` 只允许出现在设置插件管理面等）。
    但**值**是用户实际读到的文案，必须与其它文案同标准：值里出现禁用词就是文案缺陷，
    所以这里只取值、不取 key（plan §7「ui_copy_guard 扩：同时扫 dict 值」）。
    """
    out: list[str] = []
    for m in DICT_ENTRY_RE.finditer(src):
        # 组 1 = 整个带引号的值，组 2/3/4 = 三种引号内的内容
        v = m.group(2) or m.group(3) or m.group(4)
        if v:
            out.append(v)
    return out


def scan_file(path: Path) -> list[tuple[int, str, str]]:
    rel = str(path.relative_to(SRC))
    raw = path.read_text(encoding="utf-8")
    body = strip_comments(raw)
    hits: list[tuple[int, str, str]] = []
    # 文案 = JSX 文本节点 + **含中文**的字符串字面量。
    # 纯 ASCII 的字符串是技术标识（data-testid / URL / key / class），不是用户看到的字，
    # 把它们算成违规只会逼着人改测试钩子（守卫也就不再可信）。
    cjk = re.compile(r"[\u4e00-\u9fff]")
    # JSX 文本节点只有 .tsx 才有：把伪 JSX 扫描套到 .ts 上会把泛型/箭头当标签，
    # 拼出跨函数的假「文案」（实测 lib/jobs.ts 的 `Record<…>` 与 `=>` 之间被拼成一段，
    # 里面的标识符 `job` 被误判成禁用词）。.ts 只扫字符串字面量。
    if DICT_FILE_RE.search("/" + rel):
        # 词典只扫值（见 dict_values 文档）
        candidates: list[str] = dict_values(body)
    else:
        nodes = jsx_text_nodes(body) if path.suffix == ".tsx" else []
        candidates = nodes + [s0 for s0 in strings(body) if cjk.search(s0)]
    for text in candidates:
        for rx, label in FORBIDDEN:
            if label == "插件" and rel in SETTINGS_SURFACE:
                continue
            # manifest 的允许面 = 设置插件管理面 + 详情页「安装与版本」段（KI35-P4 收口）：
            # 这两处的用户任务就是核对包清单/装载态，藏掉 manifest 说不清（卡面例外）。
            if label == "manifest" and (rel in SETTINGS_SURFACE or rel == "pages/PluginDetail.tsx"):
                continue
            if rx.search(text):
                hits.append((0, label, text.strip()[:160]))
    return hits


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--quiet", action="store_true")
    ap.add_argument("--src", default=None, help="扫描根（默认 web/src；自测用它指向夹具目录）")
    args = ap.parse_args()

    global SRC
    if args.src:
        SRC = Path(args.src).resolve()

    if not SRC.is_dir():
        print(f"FAIL: 扫描目录不存在 {SRC}（守卫失效，不能算通过）")
        return 2

    files = sorted([*SRC.rglob("*.tsx"), *SRC.rglob("*.ts")])
    # i18n（i18n-G，卡 t_25f3b8c8）：词典文件**纳入扫描**，但只扫值（见 dict_values）；
    # key 是中文原文、天然含内部词（如「插件」），扫 key 会逼着人改译名或加豁免。
    # 唯一的例外是 i18n/index.tsx（基座代码：Provider/translate/hooks，无用户文案）。
    files = [
        f
        for f in files
        if not (f.relative_to(SRC).parts[:2] == ("i18n", "index.tsx"))
    ]
    all_hits: list[tuple[str, str, str]] = []
    for f in files:
        for _, label, text in scan_file(f):
            all_hits.append((str(f.relative_to(SRC)), label, text))

    # 同一文件同一段文案只报一次
    seen = set()
    uniq = []
    for rel, label, text in all_hits:
        key = (rel, text)
        if key in seen:
            continue
        seen.add(key)
        uniq.append((rel, label, text))

    scanned = len(files)
    if uniq:
        print(f"FAIL: 用户可见文案命中禁用词 {len(uniq)} 处（扫描 {scanned} 个文件）")
        for rel, label, text in uniq:
            print(f"  {rel}: [{label}] {text}")
        return 1

    if not args.quiet:
        print(
            f"OK: 用户可见文案 0 命中禁用词（行业包 / 插件 / 规格 / job / verdict / manifest）；"
            f"扫描 {scanned} 个文件（含 i18n 词典值，key 豁免）"
        )
        print(f"    例外（设置 › 插件与方案目录）：{', '.join(sorted(SETTINGS_SURFACE))}")
    else:
        print("OK: 0 命中")
    return 0


if __name__ == "__main__":
    sys.exit(main())
