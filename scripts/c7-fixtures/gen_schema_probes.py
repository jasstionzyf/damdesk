#!/usr/bin/env python3
"""C7 spike：生成「故意写错」的 pack 探针夹具（scripts/c7-fixtures/schema-probes/）。

为什么要夹具：pack 是配置驱动的对外契约，作者写错键时的**反馈质量**（响亮报错 vs 静默吞掉）
和「覆盖多少行业」同等重要。这些夹具就是用来量这件事的——每个目录只改一处，
产物由 scripts/c7_acceptance.sh 喂给 `c7_spike` 的 probes 模式。

用法：python3 scripts/c7-fixtures/gen_schema_probes.py [--check]
      --check 只校验已生成文件与期望一致（不写盘），供回归用。
"""
from __future__ import annotations

import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SRC = ROOT / "packs" / "realestate"
DST = ROOT / "scripts" / "c7-fixtures" / "schema-probes"

# (目录名, 期望的探测结论) —— 期望写进 README 供人工核对
PROBES = {
    "01-typo-classic-param": "装载通过；跑起来 classic 通道响亮报「参数错误」（deny_unknown_fields）",
    "02-unknown-detector": "装载通过；跑起来响亮报「未知检测器」；一条坏规则吞掉整张图全部判定",
    "03-pipeline-free-text-key": "装载通过；pipeline.yaml 的自由文本键被 serde 静默丢掉（schemaDiff 抓到）",
    "04-rule-typo-field": "装载通过；规则级拼错键被 serde 静默丢掉（schemaDiff 抓到）",
    "05-vl-unknown-param": "装载通过；vl 通道不查检测器名、不校验 params 键 → 未知键静默忽略",
}


def write(d: Path, name: str, text: str) -> None:
    d.mkdir(parents=True, exist_ok=True)
    (d / name).write_text(text, encoding="utf-8")


def build(dst: Path, check_only: bool = False) -> int:
    taxonomy = (SRC / "taxonomy.yaml").read_text(encoding="utf-8")
    rules = (SRC / "rules.yaml").read_text(encoding="utf-8")
    pipeline = (SRC / "pipeline.yaml").read_text(encoding="utf-8")
    report = (SRC / "report.yaml").read_text(encoding="utf-8")
    probes = {}

    # 01：把 F1 的参数键拼错一分（值域正确、键名错）
    probes["01-typo-classic-param"] = {
        "taxonomy.yaml": taxonomy,
        "rules.yaml": rules.replace(
            "check: {detector: sharpness, params: {min: 100}}",
            "check: {detector: sharpness, params: {min_var: 100}}  # 故意拼错键名",
        ),
        "pipeline.yaml": pipeline,
        "report.yaml": report,
    }

    # 02：只留 F3（未注册的 phash_dup 检测器），其余删掉 —— 隔离「坏规则吞掉整张图判定」
    probes["02-unknown-detector"] = {
        "taxonomy.yaml": taxonomy,
        "rules.yaml": """# 探针 02：未注册检测器（phash_dup 在 engine-classic 注册表里不存在）
- id: F3_duplicate
  name: 重复房照（探针：未注册检测器）
  executor: classic
  evidence: image_only
  check: {detector: phash_dup, params: {group: within_batch}}
  fixer: null
  on_fail: pass
""",
        "pipeline.yaml": """steps:
  - {kind: judge, rule: F3_duplicate}
recheck_max: 1
""",
        "report.yaml": "summary: [合格, 修复后合格, 转人工, 失败]\ndetail_columns: [imageSN, F3_duplicate]\n",
    }

    # 03：pipeline.yaml 里塞草稿式自由文本键（schema 不认）
    probes["03-pipeline-free-text-key"] = {
        "taxonomy.yaml": taxonomy,
        "rules.yaml": rules,
        "pipeline.yaml": pipeline.replace(
            "recheck_max: 1", "short_circuit: 无硬依赖，四条独立可并行  # 故意写错：不是 schema 键\nrecheck_max: 1"
        ),
        "report.yaml": report,
    }

    # 04：规则级拼错键（evidance）—— 值与键名都好，键名不是 schema 的
    probes["04-rule-typo-field"] = {
        "taxonomy.yaml": taxonomy,
        "rules.yaml": rules.replace(
            "  evidence: image_only\n  # min=100",
            "  evidance: image_only  # 故意拼错（少了 e）\n  # min=100",
        ),
        "pipeline.yaml": pipeline,
        "report.yaml": report,
    }

    # 05：vl 规则多塞一个 group_by（引擎只读 prompt/expect）
    probes["05-vl-unknown-param"] = {
        "taxonomy.yaml": taxonomy,
        "rules.yaml": rules,
        "pipeline.yaml": pipeline,
        "report.yaml": report,
    }

    rc = 0
    for name, files in probes.items():
        d = dst / name
        for fname, text in files.items():
            p = d / fname
            if check_only:
                if not p.is_file() or p.read_text(encoding="utf-8") != text:
                    print(f"DRIFT {name}/{fname}")
                    rc = 1
            else:
                write(d, fname, text)
        if not check_only:
            print(f"wrote {d.relative_to(ROOT)}/（{'/'.join(sorted(files))}）")

    readme = "\n".join(f"- `{k}/` — {v}" for k, v in PROBES.items())
    body = (
        "# C7 schema 探针夹具（故意写错的 pack）\n\n"
        "由 `gen_schema_probes.py` 生成（`--check` 校验漂移），喂给 `c7_spike` 的 probes 模式。\n"
        "用途：量「作者写错时引擎响不响」——这是换行业承诺的另一半。\n\n" + readme + "\n"
    )
    p = DST / "README.md"
    if check_only:
        if not p.is_file() or p.read_text(encoding="utf-8") != body:
            print("DRIFT README.md")
            rc = 1
    else:
        write(DST, "README.md", body)
    return rc


if __name__ == "__main__":
    sys.exit(build(DST, check_only="--check" in sys.argv))
