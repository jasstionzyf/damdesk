# 官方电商行业包（`com.soujpg.pack.ecommerce`）

电商主图合规判定 + 几何修复的**官方行业包**，按 `docs/industry-pack-protocol.md`（api_version 1.1）
以子进程插件形态运行：宿主 `pack-host` 用 stdio 行式 JSON-RPC 拉起 `plugin/__init__.py`，
算力全部经宿主能力网关的 13 个端点，包内零第三方依赖。

```
packs/plugins/ecommerce/
├── manifest.json          唯一配置面（id/version/api_version/entrypoint/capabilities/resources）
├── requirements.txt       空（刻意保持零依赖，见文件头注释）
├── README.md
├── plugin/
│   ├── __init__.py        协议壳 + judge/repair 编排（导出 PACK_INFO / HANDLERS / JUDGE / REPAIR）
│   ├── ruleset.py         六条规则 + 阈值 + 纯判定函数（测量值 → verdict，含 VL 应答映射）
│   ├── geometry.py        几何修复闭式解（crop → letterbox → upscale）
│   └── maskpng.py         纯 stdlib 写 8bit 灰度掩码 PNG（给 /op/inpaint）
├── resources/
│   ├── rules.json         规则/阈值的人读副本（单测钉住与 ruleset.py 一致）
│   └── taxonomy.json      受控词表（R5 打标用）
└── tests/                 164 个单测 + venv 隔离自检 + 行为化假网关
```

一键自检：`bash packs/plugins/ecommerce/tests/run_tests.sh`（单测 → conformance 12 条 → 包内 venv 隔离）。

## 六条规则（判什么、修什么）

| ruleId | 判据 | 通道 | 修复 |
|---|---|---|---|
| `R0_product_main` | 领域 gate：画面里有没有**可售卖、完整、单一**的商品主体。**三项硬否决**（多主体 / 主体被裁切 / 生活场景陪衬，参数在 `resources/rules.json`）+ **确定性前置**（u2net 面积 ≥15% 成团 → pass；**≤2% → `skipped`：本包对这张图不适用**）+ 边界区间 / 贴边 / 散碎 / 整幅掩码 VL 结构化事实 + **3 票多数票** | `detect.bbox`/`detect.u2net` + `vl.judge`（prompt v3，结构化 JSON；temperature 0 / top_p 1） | 无（R0 不是 pass ⇒ 其余规则短路 `skipped`） |
| `R1_size` | 短边 ≥ 1600 | `io.read` 宽高（确定性） | 几何链末段 `upscale` |
| `R2_white_bg` | 背景纯白（偏差 ≤ 8） | 宿主白度统计（`io.read` 的 `measure:["white"]`，或 `/detect/white_stats`；同一份 classic `white_edge` 实现 → 确定性、**不占 VL**），拿不到统计才退 `vl.judge` | `replace_bg`（只换底，不动商品本体） |
| `R3_no_watermark` | 文字区性质 + 面积占比（详见下节） | `detect.ocr` 定位 + `detect.bbox` 排除吊牌 + 必要时 `vl.judge` 裁量性质 | OCR 框 → 掩码 PNG → `inpaint`（只擦叠加物，不重绘本体） |
| `R4_subject_ratio` | 主体占比 0.55~0.85 | `detect.u2net`/`detect.bbox` 的 `fg_ratio`（确定性） | 几何闭式解 `crop(主体紧框) → letterbox(可选) → upscale` |
| `R5_attribute` | 打标（**不是判定**） | 默认 `skipped`；`options.attribute=true` 才问 VL | 无 |

执行序 = `PIPELINE_ORDER`（R0 gate 首位，R4 在 R3 前），两条短路语义与旧引擎一致：
**R0 不是 pass ⇒ 其余全部 `skipped`**；**R1 fail ⇒ R4 `skipped`（`depends_on`）。

## R3 判什么（K07-FIX-E / 用户裁决 D1=A）

旧语义是「OCR 认出任何字就 fail」，把商品自带的吊牌、型号码、洗水标全判成牛皮癣
（K07 门禁 7 行硬不一致）。现在的 OCR **只做候选定位**，判据是「文字区性质 + 面积占比」：

| 文字区 | 判定 |
|---|---|
| 无文字 | `pass`（`no_text`，不问 VL，省一次云调用） |
| 单字符/空文本行（图案误检，`min_text_chars=2`） | 剔除为噪声（既不判 fail，也不进 inpaint 掩码） |
| 框覆盖**主主体**紧框面积 ≥ **8%** | `fail`（`text_covers_subject`，叠加水印） |
| 框中心落在**任一商品实例**框内（吊牌/型号码/商品自带 logo） | `pass`（`subject_label_text`，排除） |
| 主体外文字面积 ≥ **2%** | `fail`（`promo_text_area`，促销条/牛皮癣） |
| 主体外文字面积 < 2% | 交 VL 裁量文字性质（`small_outside_text`）→ `pass`/`fail`/`undetermined` |

主体几何来自 `/detect/bbox`：`dominant_bbox`（主主体）+ `instances`（**全部商品实例**，掩码每个
4-连通域一个框，宿主按占画面 ≥1% 过滤噪声）。归属看框中心落在哪块实例上；水印判据只用主主体
（小实例的分母太小，拿它判「覆盖 8%」会把商品自带文字算成 100% 覆盖）。旧宿主（协议 1.0 网关）
没有 `/detect/bbox` 时退回「只按面积判」，缺 `instances` 时退回单主体口径，都在 `evidence.source`
里标出来（`ocr` / `ocr+subject_bbox`，不假装能排除吊牌）。

多商品图（K07 f002 咖啡胶囊：主玻璃杯 + 一簇胶囊）是「必须多实例」的反例：胶囊簇是独立连通域，
只认主主体时其型号码（3 行 OCR 合计 3.39%）被算成主体外促销条 → 误判 fail；按实例集合判 → pass。
逐行证据见 `evidence.subject_boxes` / `subject_count`。

**真实语料标定**（2026-10-21，4 张 pass 样本 + f004/f005，真 ppocrv6 + 真 u2net 掩码）：
判 pass 7/7 与旧引擎 R3 结论一致，K07 的 7 行硬不一致消除。

| 资产 | OCR 行 | 噪声行 | 计分行 | 文字占比 | 判定 |
|---|---|---|---|---|---|
| a001 | 3 | 2 | 1（`C214` 在主体内） | 0.73% | pass `subject_label_text` |
| a002 | 2 | 2（`P`/`0` 花纹误检） | 0 | 0% | pass `no_text` |
| a003 | 39 | 14 | 25（全部主体内） | 0.56% | pass `subject_label_text` |
| a004 | 45 | 22 | 23（全部主体内） | 3.0% | pass `subject_label_text` |
| a005 | 65 | 55 | 10（全部主体内） | 4.0% | pass `subject_label_text` |
| f004 | 2 | 2 | 0 | 0% | pass `no_text` |
| f005 | 20 | 1 | 19（全部主体内） | 19.2%（单框覆盖 <8%） | pass `subject_label_text` |

a002 那两只大框（合计占画面 42%）正是「只按面积/覆盖判水印」会误杀干净商品图的证据，
噪声过滤与 8% 覆盖门槛缺一不可。
修复只擦**该擦的框**（覆盖主体的水印 + 主体外促销条），主体内吊牌不进掩码 ——
抹掉吊牌等于篡改商品本体（合规红线）。

## R0 gate 判什么（K07-FIX-I 起：可售 + 完整 + 单一，三项硬否决）

**语义**（用户裁决 2026-10-22，f001 假阳性收口）：画面里要有**一件可售卖、完整、单一**的商品
作为主体。白底图、模特穿着/使用该商品的图、细节特写图算「有主体」；
人像写真、风景、没有商品的实景、截图、纯文字算「没有」。

**三项硬否决**（参数在 `resources/rules.json` 的 `rules[].params`，代码只执行不写死数字）：

| 否决 | 判据（schema 参数） | 为什么不放给模型裁量 |
|---|---|---|
| ① 主体数量 | 除主体外的显著物体面积 / 主体面积 ≥ `foreign_object_ratio_max`（0.30）→ `fail` | 面积比是可算的量；「还有一件差不多的东西」必须能被复现 |
| ② 主体完整性 | 主体框任一边距画面边界 < `edge_margin_ratio`（0.02 短边）**且 VL 确认有主体** → `fail`；VL 判 `cut=true` 同样算 | 贴边 = 商品出画，主图必须完整；但贴边本身不是充分条件（截图/整屏 UI 的掩码也贴边），故要 VL 确认主体在场 |
| ③ 场景度 | 生活场景陪衬物个数 > `scene_prop_max`（0）→ `fail`（桌面/餐具饮具/人手/家具/植物/包装箱/装饰摆件） | 「桌面上一拍」不是主图，这是 f001/f003 判 fail 的主因 |

**不构成 R0 否决**：白底、促销文字/水印/logo、商品自带吊牌、模特穿着或使用该商品、纯色背景
（分别由 R2/R3 判）。

**流程**分两段（`ruleset.classify_domain` + `ruleset.eval_domain_answer`）：

| 主体测量 / VL 事实 | R0 结论 | evidence |
|---|---|---|
| 面积 ≤ **2%** | `skipped`（`no_subject_area`，零 VL）——**画面里没有可判主体 = 本包对这张图不适用**，不是「不合格」 | `decided_by=subject_absent` / `applicable=false` / `score=null` |
| 面积 ≥ **15%**、掩码成团（填充度 ≥0.25、bbox 未铺满整幅）且**主体框不贴边** | `pass`（零 VL） | `decided_by=subject_area` / `vl_calls=0` |
| 面积 2%~15% / 掩码散碎 / 铺满整幅 / **主体框贴边** / 拿不到测量 | VL 给结构化事实（`subject`/`others`/`verdict`），三项否决由代码执行，3 票多数票 | `decided_by=vl_veto` + `reasons` + `votes` + `sample_verdicts`；贴边档带 `confirm_note=subject_cut_edge` |
| ↑ VL 明确回 `subject.present=false`（画面里没有商品主体） | `skipped`（`vl_not_applicable` / `vl_majority_vote_not_applicable`）——不适用，不算 fail | 无 `score`（不给 0.000 假分） |

**「不适用」= skipped，不是 fail**（卡 task）：领域 gate 问的是「本包对这张图适用吗」。
没有可判主体（面积 ≤2% 或 VL 报 `present=false`）时判 `skipped`：不进 pass/fail 统计、
不给分数/把握度、inspector 上不显示为「不合格」（宿主对 skipped 行不进低置信度队列）。
真不合格（有商品但被裁 / 多主体 / 生活场景陪衬）仍是 `fail` —— 语义不变。
**主体框贴边改为 VL 复核**（不再零 VL 直接 fail）：非商品图（截图 / 整屏 UI / 拼贴）的
u2net 掩码同样会「贴着画面边、面积 60%+、填充度 0.97」（实测 `shot-a.png` fg_ratio=0.630、
bottom 边距 0.0），纯几何判据分不出「商品被裁」与「整张图是个界面」；模型确认有主体且被裁才 fail。

**VL 段的固定化**（K07-FIX-H 起，K07-FIX-I 补多数票与结构化）：

1. **prompt 版本化**：`ecom.r0_domain.v3`（结构化 JSON 应答），evidence 里落
   `prompt_id` / `prompt_sha256`（**渲染后**正文的 sha）/ `temperature:0` / `top_p:1` / `max_tokens` / `samples`；
   prompt 正文本身在 `resources/rules.json` 的 `prompts{}`（可版本化字段），阈值占位符按生效参数渲染；
2. **解码参数固定**：`temperature=0` + `top_p=1`（宿主 `compliance_vl::chat_inner` 写死）、`max_tokens=4096`；
3. **3 票多数票**：`/vl/judge` 的 `samples=3`（宿主第 2..3 份绕过缓存取新样本），
   过半采纳（`note=vl_majority_vote`），平票 → `undetermined(vl_no_majority)`；
   票数 + 逐票理由（`sample_verdicts[].reasons`）落 evidence 与插件日志（stderr），
   既能复现也能追「哪一票为什么判 fail」。

阈值口径（用户裁决 `damdesk-repo/references/fix-policy-and-a4.md`）：主体占比下限 **0.55**
（0.85 只是平台文案目标，不可落地）、上限 0.85；短边 1600。旧引擎 main 上仍是 0.65，
对账时可用 `options.thresholds` 临时切口径，不必改包重发：

```json
{"options": {"thresholds": {"R4_subject_ratio": {"min": 0.65}}}}
```

## 修复：为什么是闭式解而不是「单步 smart_crop」

旧引擎每规则各自「检测 → 修 → 只复检本条」，裁完把白边/尺寸弄坏也没人管（A4 修好后 A2/A3 必坏）。
本包的 `repair` 是**一条链、受影响规则复判、轮次硬上限 3**：

```
measure(全部规则) → 取 fail 集合 → 几何闭式解(crop/letterbox/upscale) → 换底 → OCR 局部 inpaint
                        ↑                                                        │
                        └──── 每轮只复判「被改动波及」的规则（AFFECTED_RULES）────┘
```

增量复判（K07-FIX-D）：第 1 轮全量、之后按 `ruleset.AFFECTED_RULES` 只复判受影响的规则
（几何 → 尺寸/白底/占比；换底 → 白底；inpaint → 水印），收尾再补一次「最近一次仍 fail」
的最小集。同一算子重复做且结论没变 → 判 `repair_not_converged` 直接收手，
不把「补不干净」重复到第 3 轮（实测一次 `inpaint` ≈4.6 s、一次 OCR ≈2.0 s，空转两轮就是十几秒）。

几何闭式解（`geometry.solve`）：

几何闭式解（`geometry.solve`，**bbox 驱动**，K07-FIX-E / 用户裁决 D5=A）：

```
crop_box   = 宿主窗（/detect/bbox 带 target_ratio）> 主主体紧框 dominant_bbox > 居中推定
A          = fg_ratio × W × H            主体像素面积（只做测量，不参与窗口推导）
predicted_square = A / max(crop_w, crop_h)²     补白成 1:1 后的占比
predicted_aspect = A / (crop_w × crop_h)        不补白（保持窗口比例）的占比
letterbox  = predicted_square 仍 ≥ 下限时才补白；否则保留窗口比例（evidence: letterbox_skipped）
scale      = 1600 / 短边                  （短边不足门槛时才放大，短边不是长边）
```

为什么必须 bbox 驱动：只拿 `fg_ratio` 标量做 `M = sqrt(A/0.75)` 只能**居中推定**，
主体偏心/横跨画面的图（a002 主体横跨 93% 宽）会被裁掉一部分，占比 0.445 → 0.032，
第二轮再按变小的占比收紧 —— 这就是 K07「修 3 轮把主体裁没」的根因（`KI-24`）。
改成裁主主体紧框后，同 4 张失败样本一次就落到 0.715/0.557/0.614/0.695（真裁真测
0.716/0.667/0.612/0.678），短边全部 ≥1600（回归单测 `test_k07_four_failed_samples_regression`）。

- **占比已达标就不许乱裁**（避免把合格图裁坏）；只有「补白成正方形会把占比压破下限」时才收窗。
- **补白是可选动作**：宽主体图补成 1:1 会把占比压破下限（0.715 → 0.385），此时不补白、
  保留窗口比例，并在 `trail` 里落 `letterbox_skipped` + 被跳过的占比，便于人工核对。
- **保不住主体就判不可解**：裁到主体紧框仍低于下限 → `needs_human:subject_ratio_below_min_keeping_subject`，
  绝不靠继续切主体去凑占比（三态口径，交人工/重拍）。
- **主体占满（> 0.85）改为扩窗**：向主体周围纳入背景把占比压回 target；画面不够大才判
  `subject_ratio_above_max`。
- 每轮都重新测量（真网关按像素算 `fg_ratio`），最多 3 轮收手（`test_never_converging_stops_at_max_rounds`）。
- 产物命名 `<ruleId>_<W>x<H>.png`，每份产物都对应一条 `fixed=true` 的 verdict（conformance 11 验这条）。

## 能力缺口（协议 1.1 已补齐两条）

协议 1.1 起网关补了两个端点，本包都已改用，1.0 时代的「口径改写」不再是默认路径：

| 原缺口 | 1.1 的端点 | 本包现在怎么用 | 旧宿主（1.0 网关）时的降级 |
|---|---|---|---|
| 无像素统计端点，旧引擎 classic `white_edge`（边缘白度偏差）复现不了 | `POST /detect/white_stats`（能力 `detect.white`，复用 `engine-classic::detectors::white_edge` 同一份实现 → 与旧引擎逐值同口径、确定性） | `R2_white_bg` 直接吃 `mean_dev` 判 `pass/fail`，`score` 语义与 classic 一致 | 端点/能力缺失（403 或 404 no_such_endpoint）→ 退回 `vl.judge`，在 `evidence.source` 标 `vl_fallback_no_white_stats`（不假装 classic） |
| 无主体 bbox 端点，掩码只回 `mask_path`，插件不解码大图（零依赖），裁剪窗只能**居中**推定 | `POST /detect/bbox`（能力同 `detect.u2net`：同一次推理，带 `target_ratio` 时回 `window` = 主主体锚定、占比二分到目标的窗） | 几何修复直接用宿主窗（`anchor=window`）；只有 bbox 时按主体锚点裁（`anchor=bbox`），并保证覆盖主体紧框 | 退回 `/detect/u2net` 的 `fg_ratio` 居中推定（`anchor=center`，≤3 轮实测纠偏） |

宿主对同一张图连调 `/detect/u2net` 与 `/detect/bbox` 只跑一次 u2net（网关按路径+mtime 缓存最近一次掩码）。

## 包内 venv（协议 §4.0）

宿主为每个包建独立 venv：`<runtime>/python -m venv <pack>/venv && <pack>/venv/bin/pip install -r requirements.txt`。
本包 `requirements.txt` 为空，所以这一步在断网环境也瞬间完成、`rm -rf venv` 即卸载干净；
`tests/run_tests.sh` 的第 ③ 段会真的建一个 venv 并把 conformance 用 venv 解释器再跑一遍，
并断言 venv 里没有 numpy/PIL/cv2/requests（证明插件不偷宿主 site-packages）。

## 已知限制

- `R3` 的 2%/8% 阈值是**策略值**（用户裁决口径），不是从语料统计反推的常量：
  主体外文字 2% 以下、主体内文字 8% 以下的边界情形一律交 VL 裁量（每次 ~1–2s 云调用），
  所以「有少量角标」的图仍是 R3 里最贵的一档。阈值可用
  `options.thresholds.R3_no_watermark` 临时切口径。
- 旧宿主（协议 1.0 网关，无 `/detect/bbox`）没有主体框：R3 只能按面积判（不排除吊牌），
  R4 退回居中推定 —— 两条都在 `evidence.source` / `anchor` 里标出来，不假装等价。
- `R5` 打标默认关闭（打标不是判定，且 VL 调用花钱）；开启需 `options.attribute=true`。
- 维修产物只写 `options.workdir`（宿主给的写根），包内不碰其他路径。
- `tools/pack-sdk-python/examples/ecommerce_py` 是 SDK 的写法范本（另一张卡的领地），
  其 R3/R0 仍是旧语义；它的一致性脚本按 K00 基线回放，与本包的判定改动互不影响。
  同步新语义需要单独改 SDK 样例包（不在本包内）。
