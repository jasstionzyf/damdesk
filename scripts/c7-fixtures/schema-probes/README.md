# C7 schema 探针夹具（故意写错的 pack）

由 `gen_schema_probes.py` 生成（`--check` 校验漂移），喂给 `c7_spike` 的 probes 模式。
用途：量「作者写错时引擎响不响」——这是换行业承诺的另一半。

- `01-typo-classic-param/` — 装载通过；跑起来 classic 通道响亮报「参数错误」（deny_unknown_fields）
- `02-unknown-detector/` — 装载通过；跑起来响亮报「未知检测器」；一条坏规则吞掉整张图全部判定
- `03-pipeline-free-text-key/` — 装载通过；pipeline.yaml 的自由文本键被 serde 静默丢掉（schemaDiff 抓到）
- `04-rule-typo-field/` — 装载通过；规则级拼错键被 serde 静默丢掉（schemaDiff 抓到）
- `05-vl-unknown-param/` — 装载通过；vl 通道不查检测器名、不校验 params 键 → 未知键静默忽略
