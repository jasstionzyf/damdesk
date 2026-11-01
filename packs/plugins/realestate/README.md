# 房产行业包（官方包）—— `com.soujpg.pack.realestate`

房源照片入库前审：**先判「这是不是房源图」，再判清晰/曝光/重复/场景齐套/尺寸**。
迁移来源：`packs/realestate/{rules,pipeline,taxonomy}.yaml`（旧引擎声明，C7 spike 产物），
形态按 2026-10-20 裁决改为 **Python 源码包**（宿主零改动）。

```
manifest.json            # 唯一配置面（协议 v1.0）
requirements.txt         # 空（零第三方依赖，理由见文件内注释）
plugin/__init__.py       # 协议壳 + 网关客户端 + judge/repair 编排 + 组聚合与缓存
plugin/ruleset.py        # 规则/阈值/提示词 + 纯判定函数（不碰 IO）
plugin/imagedec.py       # 纯标准库解码（PNG + baseline JPEG）→ Rec.709 luma
resources/rules.json     # 人读副本（单测 test_rules_json_matches_ruleset 钉一致性）
resources/scenes.json    # 场景词表（同上钉一致性）
tests/                   # 46 例单测 + 全链路 + 语料矩阵脚本 + 夹具
```

自检：`bash tests/run_tests.sh`（单测 + conformance 12/12 + 矩阵冒烟）。

## 规则（6 条）

| 规则 | 执行体 | 作用域 | 确定性 | 修复器 | 不通过 |
|---|---|---|---|---|---|
| F0_domain | VL 词表（是/否） | 逐图 | 否 | — | fail（非房源图，后续全 skipped） |
| F5_min_size | 元数据（长边≥800 且短边≥600） | 逐图 | 是 | `upscale`（几何） | auto_fix |
| F1_sharpness | 包内解码 + 拉普拉斯方差 ≥100 | 逐图 | 是 | — | fail / 转人工 |
| F2_exposure | 包内解码 + 死区占比 ≤0.30、均值亮度 30~245 | 逐图 | 是 | — | fail / 转人工 |
| F3_duplicate | 包内 pHash（同目录组）汉明 ≤8 | 按组 | 是 | — | **仅标记**（`on_fail: pass`） |
| F4_scene_complete | VL 逐图归类 → 组内齐套 | 按组 | 否 | — | fail / 转人工 |

阈值与来源（`packs/realestate/rules.yaml`，C7 真图标定）：
- F1 `min=100` ≈ 真图 p5（只拦明显糊的）；F2 `max_dead_ratio=0.30` ≈ p73、均值 `[30,245]`；
- F3 `threshold=8`（pHash 汉明 ≤8 判近似重复）；
- F4 `require_all=[客厅,卧室,厨房,卫生间]`；
- F0 提示词 = COMPLIANCE-DOMAIN 定稿 v4（132 张真图 4 轮迭代），**原样搬过来，未改一字**。

## 三态口径（不许冒充）

- VL 不可判 / 词表外答案 / 解不出像素 → `undetermined` + 原因（`note`），绝不默认 pass；
- **不可修复的失败一律转人工**：`repair` 把 F0~F4 的 fail 改写成
  `undetermined + note=needs_human:<原因> + label=manual_review_required`，不自动放行；
- F3 是软规则：判 fail 但 `evidence.soft_rule=true` 且 note 明写「仅标记，不判不合格」；
- 领域 gate 非 pass 时后续规则 `skipped`（`note=domain_gate_<status>`），既不烧 VL 也不算合格。

## 与旧引擎的差异（三处，全部是形态所迫或卡面要求，不是语义漂移）

1. **F1/F2 的像素测量在包内算**：宿主网关 v1.0 的 10 个端点里没有 classic 像素统计。
   三条备选都否掉了（改宿主加端点 = 破坏「宿主零改动」；走 VL 判清晰度/曝光 = 丢确定性；
   requirements 加 Pillow = 包内 venv 背 C 扩展与平台 wheel）。于是包内自带纯标准库解码器，
   luma 口径与 `crates/engine-classic/src/detectors.rs` 逐项对齐（Rec.709 系数、拉普拉斯核
   `[0,1,0;1,-4,1;0,1,0]`、死区边界压线即死区、总体方差分母）。
   实测偏差（真实语料 14 张 + 合成夹具，见 `tests/fixtures/pillow_reference.json`）：
   拉普拉斯方差 +0.1%~+2.2%、死区占比 ≤0.006、均值亮度 ≤0.5 级；饱和色合成图最差 +15%（色度上采样策略差异，回归钉已记录）。
2. **F3/F4 的组作用域由插件自聚合**：协议 `judge` 是逐资产调用，没有 `kind=compliance_group`
   的组级 job。插件把「资产所在目录」当组（`options.group_dir` 可覆盖），逐成员测量后本地汇总；
   按 C11 口径**排除已被本进程判为「非房源」的成员**（结果缓存在 `DAMPACK_WORK_DIR`），
   所以一个组要在成员陆续判定后才收敛（矩阵脚本跑两轮取收敛值）。
3. **新增 F5_min_size**：卡面要求本包具备「仅几何修复」能力面，而 F0~F4 的失败**都不可算法修复**
   （rules.yaml 裁决：房源图修图=失真，一律人工）。没有可修规则时，conformance 的
   「repair 产物落盘」断言无实现可验。F5 取平台上传最低档 **800×600（长边≥800 且短边≥600）**，
   修复只有 `upscale`（几何，`op.generative` 未声明 → 生成式重绘在本包里根本调不到）。
   **这是本形态新增的规则，旧引擎没有对应项**；F0~F4 的语义与阈值一字未改。

## manifest 的 `deterministic: false`（与卡面一句话的冲突说明）

卡面（K12）写 `manifest: deterministic: true`，但协议 §7 铁律 2 规定**任何含 VL 的包必须
声明 `deterministic: false`**，本包含 F0（领域 gate）与 F4（场景归类）两条 VL 规则，
且旧引擎 `rules.yaml` 对这两条的 `deterministic` 也是 false。故按协议声明 false，
并在 `pack.info.rules[].deterministic` 上**逐规则**给出确定性（F1/F2/F3/F5 = true）——
宿主硬判定按逐规则位走，不受整包位影响。这是一处经确认的口径修正，不是漏项。

## 能力面（manifest.capabilities）

`io.read`（元数据 + 可读性校验）、`op.geometric`（仅 F5 的 upscale）、
`vl.judge`（F0/F4）、`report.emit`（job 账本）。
**不声明** `op.generative`（房源图禁止生成式重绘，声明了就等于给了口子）、
`detect.u2net`/`detect.ocr`（本包不需要主体掩码与文字检测）。

## 性能（1000px 级房源图，实测）

| 步骤 | 耗时 |
|---|---|
| 全解码 + 拉普拉斯/曝光（F1/F2） | 0.45~2.0s |
| DC 档解码 + pHash（F3 每成员） | 0.06~0.13s |
| VL 调用（F0 每图 / F4 每成员首次） | 1~3s（缓存命中 0s） |
| 单张全量 judge（缓存热） | 1.0~2.2s |
| 单张全量 judge（组内首次，含 F4 全组归类） | ≤25s（timeout_secs=180 内有充足余量） |

解码规模上限 `MAX_PIXELS=24M`；超出 → F1/F2 落 `undetermined(asset_too_large)` 转人工。

## 已知限制

- 解码器只支持 **baseline/extended sequential JPEG** 与**非交错 PNG**；
  progressive JPEG / lossless JPEG / Adam7 PNG → `undetermined(asset_decode_failed)`，转人工，不猜。
- 组内成员上限 `GROUP_MEMBER_MAX=24`（一个目录几千张时只取前 24 个，其余不计入证据）。
- F4 依赖 VL 逐成员归类：组大且缓存冷时单次 judge 会慢（受 `timeout_secs=180` 约束）。
