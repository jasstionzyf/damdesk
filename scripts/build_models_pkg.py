#!/usr/bin/env python3
"""构建 damdesk 增强模型分发物料（extras zip + models-manifest.txt + 代码一致性校验）。

布局契约（与 crates/engine-embed/src/model_store.rs 对齐，两者不一致即失败）：
- 落盘布局 = `{models_root}/<manifest.name>/<file.name>`（ModelStore::file_path）
- zip 布局 = 落盘布局：zip 解压到 models root 后逐文件 bytes/sha256 校验通过（ready=true）
- 下载 URL = `base_url + "/" + url_path`；url_path 形态与目录名不一致（u2net 等单文件
  模型是顶层文件名）——HTTP 下载源需按 url_path 形态另摆树，本脚本输出该清单供
  publish 脚本在 COS/HF 上补齐（zip 树本身不受影响）。

用法：
  python3 scripts/build_models_pkg.py                 # 全量构建（硬链接，不占额外空间）
  python3 scripts/build_models_pkg.py --check-only    # 只做一致性校验，不写任何文件
"""
from __future__ import annotations

import argparse
import hashlib
import os
import re
import sys
import zipfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
MODEL_STORE_RS = REPO / "crates/engine-embed/src/model_store.rs"

# 打包清单（name → (源路径相对 --src, license, source-url)）。与任务表逐字对应；
# dinov3（Meta gated）与 siglip2（随安装包内置）显式排除。
MODELS: dict[str, tuple[list[tuple[str, str]], str, str]] = {
    # file_name: (zip 内相对路径, 源路径相对 --src)
    "u2net": (
        [("u2net.onnx", "u2net/u2net.onnx")],
        "Apache-2.0",
        "https://github.com/xuebinqin/U-2-Net",
    ),
    "ppocrv6-medium-det": (
        [("inference.onnx", "ppocrv6-medium/det/inference.onnx")],
        "Apache-2.0",
        "https://github.com/PaddlePaddle/PaddleOCR",
    ),
    "ppocrv6-medium-rec": (
        [
            ("inference.onnx", "ppocrv6-medium/rec/inference.onnx"),
            ("ppocrv6_dict.txt", "ppocrv6-medium/rec/ppocrv6_dict.txt"),
        ],
        "Apache-2.0",
        "https://github.com/PaddlePaddle/PaddleOCR",
    ),
    "realesr-general-x4v3": (
        [("realesr-general-x4v3.onnx", "realesr-general-x4v3/realesr-general-x4v3.onnx")],
        "BSD-3-Clause",
        "https://github.com/xinntao/Real-ESRGAN",
    ),
    "lama-fp32": (
        [("lama-fp32.onnx", "lama-fp32/lama-fp32.onnx")],
        "Apache-2.0",
        "https://github.com/advimman/lama",
    ),
}
MANIFEST_NAME = "models-manifest.txt"
ZIP_NAME = "damdesk-models-extras-v1.zip"
FORBIDDEN_PREFIXES = ("dinov3", "siglip2")  # 硬边界：不得出现在任何包内


def parse_model_store_rs(path: Path) -> dict[str, list[dict]]:
    """解析 model_store.rs::manifests() 返回 {model_name: [{name,url_path,bytes,sha256}]}。

    只做正则提取（不引入 Rust 工具链），解析不到或缺失字段即抛错。
    """
    text = path.read_text(encoding="utf-8")
    # 按 `pub fn xxx() -> ModelManifest {` 切块，块体到下一个 `pub fn` 为止
    chunks = re.split(r"pub fn (\w+)\(\) -> ModelManifest \{", text)
    out: dict[str, list[dict]] = {}
    for i in range(1, len(chunks) - 1, 2):
        fn_name, body = chunks[i], chunks[i + 1]
        body = body.split("\npub fn ", 1)[0]
        m = re.search(r'name: "([^"]+)"\.to_string\(\)', body)
        if not m:
            continue  # name 走常量引用（siglip2/dinov3-vits16）→ 非本包分发对象，跳过
        model_name = m.group(1)
        files = []
        for fb in re.findall(r"ModelFile \{(.*?)\}", body, re.S):
            def g(k: str, quoted: bool, into: bool) -> str:
                pat = rf'{k}:\s*'
                pat += r'"([^"]+)"' if quoted else r'([\d_]+)'
                if into:
                    pat += r'\s*\.\s*into\(\)'
                m2 = re.search(pat, fb)
                if m2 is None:
                    raise SystemExit(f"FAIL: model_store.rs {fn_name}() 字段 {k} 未解析到:\n{fb[:200]}")
                return m2.group(1).strip()
            files.append(
                {
                    "name": g("name", quoted=True, into=True),
                    "url_path": g("url_path", quoted=True, into=True),
                    "bytes": int(g("bytes", quoted=False, into=False).replace("_", "")),
                    "sha256": g("sha256", quoted=True, into=False),
                }
            )
        if not files:
            raise SystemExit(f"FAIL: model_store.rs {fn_name}() 未解析到 ModelFile")
        out[model_name] = files
    if not out:
        raise SystemExit(f"FAIL: {path} 未解析到任何 ModelManifest")
    return out


def sha256_of(p: Path) -> str:
    h = hashlib.sha256()
    with p.open("rb") as f:
        for chunk in iter(lambda: f.read(4 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def check_code_consistency(rust: dict[str, list[dict]]) -> None:
    """包清单 ↔ 代码常量逐项对齐：model/file 集合、bytes、sha256、url_path 形态。"""
    errors = []
    for model_name, (files, _lic, _src) in MODELS.items():
        rs = rust.get(model_name)
        if rs is None:
            errors.append(f"代码 manifests() 缺少模型 {model_name}（pkg 有、代码无）")
            continue
        rs_by_name = {f["name"]: f for f in rs}
        for fname, _src_rel in files:
            r = rs_by_name.get(fname)
            if r is None:
                errors.append(f"代码 {model_name} 缺少文件 {fname}（下载会 ready=false）")
                continue
        for f in rs:
            if f["name"] not in {fn for fn, _ in files}:
                # 代码声明的文件包里没有 —— 只有当该模型在打包清单内才是错误
                errors.append(f"包清单缺少代码声明的文件 {model_name}/{f['name']}")
    extra = set(rust) - set(MODELS) - {"dinov3-vits16-onnx"}
    # siglip2 内置塔不进包属预期；dinov3 显式排除。其余代码模型若存在应进包。
    for name in extra:
        errors.append(f"代码新模型 {name} 未进打包清单（若属增强模型需补 MODELS 表）")
    if errors:
        for e in errors:
            print(f"  ✗ {e}")
        raise SystemExit(f"FAIL: 包清单与 model_store.rs 不一致（{len(errors)} 项）——这是防"
                         "「下载下来 ready=false」的唯一保障")


def check_pkg(pkg: Path, rust: dict[str, list[dict]] | None = None) -> dict[tuple[str, str], dict]:
    """校验 pkg 树：布局、bytes、sha256、禁入前缀。返回 {(model,file): info}。"""
    if rust is None:
        rust = parse_model_store_rs(MODEL_STORE_RS)
    infos: dict[tuple[str, str], dict] = {}
    errors = []
    total = 0
    for model_name, (files, lic, src_url) in MODELS.items():
        if any(model_name.startswith(p) for p in FORBIDDEN_PREFIXES):
            errors.append(f"禁入模型 {model_name} 出现在打包清单")
        for fname, _src_rel in files:
            if any(fname.startswith(p) for p in FORBIDDEN_PREFIXES):
                errors.append(f"禁入文件 {model_name}/{fname}")
            p = pkg / model_name / fname
            if not p.is_file():
                errors.append(f"缺文件 {model_name}/{fname}")
                continue
            size = p.stat().st_size
            digest = sha256_of(p)
            total += size
            infos[(model_name, fname)] = {
                "path": p, "bytes": size, "sha256": digest,
                "license": lic, "source": src_url,
            }
            rust_files = rust.get(model_name, [])
            r = next((f for f in rust_files if f["name"] == fname), None)
            if r is None:
                errors.append(f"{model_name}/{fname}: 代码无此文件声明")
                continue
            if size != r["bytes"]:
                errors.append(f"{model_name}/{fname}: bytes {size} ≠ 代码 {r['bytes']}")
            if digest != r["sha256"]:
                errors.append(f"{model_name}/{fname}: sha256 不一致 → 客户端会判损坏并重下")
    # 禁入前缀：全树扫一遍（防未来误放）
    for p in pkg.rglob("*"):
        rel = p.relative_to(pkg).as_posix()
        if p.is_file() and any(part.startswith(FORBIDDEN_PREFIXES) for part in rel.split("/")):
            errors.append(f"pkg 内出现禁入路径 {rel}")
    if errors:
        for e in errors:
            print(f"  ✗ {e}")
        raise SystemExit(f"FAIL: pkg 校验 {len(errors)} 项不一致")
    print(f"  pkg 校验通过：{len(infos)} 文件，合计 {total:,} B，bytes/sha256 与 model_store.rs 全一致")
    return infos


def stage_files(src: Path, pkg: Path) -> None:
    """按 <name>/<file> 摆文件：硬链接优先（同盘零拷贝），跨盘退回复制。"""
    pkg.mkdir(parents=True, exist_ok=True)
    for model_name, (files, _lic, _src) in MODELS.items():
        (pkg / model_name).mkdir(parents=True, exist_ok=True)
        for fname, src_rel in files:
            s, d = src / src_rel, pkg / model_name / fname
            if d.is_file() and d.stat().st_size == s.stat().st_size:
                if d.exists() and d.samefile(s) or sha256_of(d) == sha256_of(s):
                    continue  # 已就位（硬链接或内容一致）
            d.unlink(missing_ok=True)
            try:
                os.link(s, d)
            except OSError:
                d.write_bytes(s.read_bytes())  # 跨盘兜底（一次性，慢）


def write_manifest(pkg: Path, infos: dict[tuple[str, str], dict]) -> None:
    lines = [
        f"# damdesk-models-extras-v1 | manifest 对照表",
        "# name\tfile\tbytes\tsha256\tlicense\tsource-url",
    ]
    for model_name, (files, _lic, _src) in MODELS.items():
        for fname, _src_rel in files:
            i = infos[(model_name, fname)]
            lines.append(
                f"{model_name}\t{fname}\t{i['bytes']}\t{i['sha256']}\t{i['license']}\t{i['source']}"
            )
    (pkg / MANIFEST_NAME).write_text("\n".join(lines) + "\n", encoding="utf-8")


def build_zip(pkg: Path, infos: dict[tuple[str, str], dict], zip_path: Path) -> None:
    """zip 布局 = 落盘布局（解压到 models root 即 ready）；deflate-6，逐文件 CRC 校验。"""
    zip_path.parent.mkdir(parents=True, exist_ok=True)
    entries = [(pkg / model_name / fname, f"{model_name}/{fname}")
               for model_name, (files, _l, _s) in MODELS.items() for fname, _ in files]
    entries.append((pkg / MANIFEST_NAME, MANIFEST_NAME))
    with zipfile.ZipFile(zip_path, "w", zipfile.ZIP_DEFLATED, compresslevel=6) as z:
        for src, arc in entries:
            z.write(src, arcname=arc)
    with zipfile.ZipFile(zip_path) as z:
        bad = z.testzip()
        if bad is not None:
            raise SystemExit(f"FAIL: zip CRC 损坏 {bad}")
        names = set(z.namelist())
    expect = {a for _, a in entries}
    if names != expect:
        raise SystemExit(f"FAIL: zip 条目不一致 多={names - expect} 少={expect - names}")
    print(f"  zip 就绪：{zip_path} {zip_path.stat().st_size:,} B（{len(entries)} 条目，CRC 全过）")


def print_http_tree(infos: dict[tuple[str, str], dict]) -> None:
    """HTTP 下载源（base_url 树）需要 url_path 形态的对象；zip 树与之不同构时提示补齐。"""
    rust = parse_model_store_rs(MODEL_STORE_RS)
    print("  HTTP 下载源所需 url_path 形态（base_url 下应存在的对象）：")
    for model_name, (files, _l, _s) in MODELS.items():
        for fname, _ in files:
            r = next(f for f in rust[model_name] if f["name"] == fname)
            pkg_rel = f"{model_name}/{fname}"
            mark = "顶层" if "/" not in r["url_path"] else ""
            alt = "" if r["url_path"] == pkg_rel else f"（≠zip 布局 {pkg_rel}，需补顶层对象）{mark}"
            print(f"    {r['url_path']}  ->  {infos[(model_name, fname)]['bytes']:,} B {alt}")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--src", type=Path, default=Path("/data"), help="模型源目录")
    ap.add_argument("--out", type=Path, default=Path("/data"), help="pkg 输出目录")
    ap.add_argument("--zip", type=Path, default=Path(f"/data{ZIP_NAME}"), help="zip 输出路径")
    ap.add_argument("--check-only", action="store_true", help="只校验不写文件")
    ap.add_argument("--emit-http-tree", action="store_true",
                    help="输出 HTTP 源树 TSV（url_path\\tpkg_rel\\tbytes）供 publish 脚本消费")
    args = ap.parse_args()

    if args.emit_http_tree:
        # 纯 TSV 到 stdout：任何进度输出都不许混入
        rust = parse_model_store_rs(MODEL_STORE_RS)
        for model_name, (files, _l, _s) in MODELS.items():
            for fname, _ in files:
                r = next(f for f in rust[model_name] if f["name"] == fname)
                print(f"{r['url_path']}\t{model_name}/{fname}\t{r['bytes']}")
        return

    print(f"[1/4] 解析 {MODEL_STORE_RS.relative_to(REPO)}")
    rust = parse_model_store_rs(MODEL_STORE_RS)
    for k in MODELS:
        assert k in rust, f"代码缺模型 {k}"
    print(f"  代码 manifests() 共 7 个；字面量 name 可解析 {len(rust)} 个，打包 {len(MODELS)} 个"
          "（siglip2 内置塔与 dinov3 走常量 name 不进包）")

    print("[2/4] 包清单 ↔ 代码常量一致性")
    check_code_consistency(rust)
    print("  一致")

    if args.check_only:
        print("[3/4] 校验现有 pkg")
        infos = check_pkg(args.out)
        print("[4/4] HTTP 树形态")
        print_http_tree(infos)
        return

    print("[3/4] 摆放 pkg（硬链接）+ manifest")
    stage_files(args.src, args.out)
    infos = check_pkg(args.out)
    write_manifest(args.out, infos)
    print(f"  {args.out/MANIFEST_NAME} 就绪")

    print("[4/4] 生成 zip + HTTP 树形态")
    build_zip(args.out, infos, args.zip)
    print_http_tree(infos)
    print("OK: 全部物料构建并通过代码一致性校验")


if __name__ == "__main__":
    main()
