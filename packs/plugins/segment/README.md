# 官方抠图 / 换背景包（`com.soujpg.pack.segment`）

一句话：**把商品主体从原图里抠出来**，按 `bgMode` 输出「纯白底 JPEG / 透明 PNG / 指定色底 JPEG」。
`kinds: ["process"]` —— 只产出「处理结果 + 产物」，**没有**审核结论（不伪造 pass）。

## 1. 它解决什么

- 「白底规格」= 本插件 `bgMode=white`（抠图 + 填白），**不依赖**图片处理插件，插件之间不互相调用。
- 第三方修图/换背景场景 = 本插件 `bgMode=transparent`（得到 RGBA PNG，后续自有工具贴图）。
- 品牌色背景（京东/自建站常用）= `bgMode=color` + `color`。

## 1.5 动作面（`pack.info.actions[]`，N6e 动作链；v1.2.0 起）

宿主的动作链（`POST /api/jobs` 带 `actions[]`）按 `options.action` 选动作，**权威清单是本包
`pack.info.actions[].id`**（`crates/app-web/src/compliance_api.rs::PluginRuntime::open` 拿它做入参
校验：未声明的动作直接 400 点名）。本包只声明**一个**动作：

| `actions[].id` | 名字 | 含义 |
|---|---|---|
| `white_bg_fill` | 抠图白底 | 抠出主体并合成纯白背景；输入已是白底则跳过（零产物） |

- 透明底 / 指定色底**不编动作名**：它们是参数（`bgMode=transparent|color`），参数面已经够表达，
  多编动作只会让同一件事有两个入口。
- 清单**唯一声明处 = `resources/actions.json`**（v1.2.1 起）：插件启动时读它生成
  `pack.info.actions[]`，宿主 `spec_seed` 也读同一份来派生「官方默认方案」（图库「送去处理 →
  抠图/换背景」默认有方案可选，不用用户先自建）。写在 Python 里宿主读不到 —— 读 `pack.info`
  必须拉起子进程，而方案清单是读接口要用的廉价数据。
- `action` 刻意**不进 `params.schema`**：schema 默认值会被宿主注入每一次执行
  （`ParamSchema::resolve_json`），一旦声明了 `action` 默认值，`bgMode=transparent` 的普通规格
  也会被注入「白底动作」→ 语义被静默改掉。链内由宿主在调用时补 `options.action`
  （`PluginRuntime::action_asset`），不走 schema。
- `action` 与 `bgMode` 冲突（如 `white_bg_fill` + `bgMode=transparent`）→ `-32003`，不静默按其中一个执行。
- **为什么 v1.1.0 跑不了链**：那时 `pack.info` 没有 `actions[]`，宿主 `PackIdentityInfo.actions`
  恒空 → `validate_actions` 把任何动作名都判「未知动作」（宿主行为是对的，缺的是包的自述）。
  真机卡 `task` 修的正是这一处，包 **1.1.0 → 1.2.0**（`manifest.json` + `PACK_VERSION` 同步）。

## 1.6 链内口径：`result` 与产物的机读去处

链契约（`crates/svc-pipeline/src/compliance_chain.rs`）要求「**没产物的步必须标 `skipped`**」
（`ok` 却没 `fields.outPath` = `action_no_product` 整链失败）。因此守卫②的行有两种口径：

| 调用形态 | `result` | 产物 |
|---|---|---|
| 普通规格（无 `options.action`）→ judge | `ok` + `skipReason=already_white` | 零产物（N14 门禁按此断言） |
| 动作链（带 `options.action`） | `skipped` + `skipReason=already_white` | 零产物（链沿用上一步的图） |

`ok` 行同时给**机读产物去处**（协议 §4.4 `fields`，宿主的 `ChainStep::from_outcome` 就读这几个键）：
`outPath` / `sha256` / `out[宽,高]` / `format` / `bytes`；中文列（`背景` / `输出`）保留给人看。

## 2. 参数（`resources/params.schema.json`，宿主 `ParamForm` 通用渲染）

| 参数 | 类型 | 范围 | 默认 | 说明 |
|---|---|---|---|---|
| `bgMode` | enum | white / transparent / color | white | 背景模式 |
| `color` | color | `#rrggbb` | `#ffffff` | 仅 `bgMode=color` 时出现（`when`） |
| `feather` | integer | 0..8 px | 2 | 边缘斜坡宽度加量（见 §4） |
| `edgeThreshold` | integer | 0..255 | 128 | 掩码二值化阈值（高级折叠） |

## 3. 产物形态

| bgMode | 产物 | 说明 |
|---|---|---|
| white | JPEG | 纯白底；由**宿主** `/op/crop` 编码（质量口径归宿主，包内不写 JPEG 编码器） |
| color | JPEG | 指定色底，同上 |
| transparent | **RGBA PNG** | alpha = 掩码斜坡；RGB 是**非预乘**原色（贴到任何背景都不出白边） |

产物落在宿主注入的 `options.workdir`，文件名 `<assetId>-segment-<mode>.<ext>`；
中间件（裁块 PNG / 掩码副本 / 合成 PNG）用完即删 —— 一个资产一次一个文件，
不自建目录树、不写平行资产表。结果行按协议 §4.4：

```jsonc
{"assetId": "...", "result": "ok",
 "artifacts": [{"kind": "export_file", "path": "...", "sha256": "..."}],
 "evidence": {"fgRatio": 0.72, "maskCellPx": 19, "alphaRampPx": 42, "...": "..."}}
```

没有主体（`fg_ratio = 0` 或无 bbox）→ `result=skipped, skipReason=no_subject`
（**不**输出一张全空图冒充成功）。

## 3.5 三道守卫（「不许白干」和「不许刷坏」是同一件事的两面）

处理类包最容易犯的错不是算错，而是**该不跑的时候硬跑**。三道守卫按「先便宜后贵」的顺序排：

| # | 守卫 | 触发条件 | 返回 | 省下什么 |
|---|---|---|---|---|
| ② | **已白底跳过** | 目标底色是纯白（`bgMode=white` 或 `color=#ffffff`）且输入实测已是白底 | `result=ok` + `skipReason=already_white`，**零产物** | 一次 u2net 推理 + 两次裁块 + 一次 JPEG 编码（实测 14~19ms 就返回） |
| ① | **掩码低置信度拒绝** | `fg_ratio < 0.02` 或 `> 0.95`（u2net 把整图判成主体 / 只抓到噪点） | `result=skipped` + `skipReason=mask_low_confidence` | 不做无用合成，也不拿错掩码去刷图 |
| ③ | **主体损失红线** | 主体**不透明核心**（`alpha==255`）被改动像素 > 5% | `result=failed` + `error.code=subject_loss`，零产物 | 防坐标映射/合成错位这类回归（贴错位置、整体偏移） |

### 守卫② 的两次实测（都走宿主 `detect.white` 能力，classic `white_edge`，确定性、不占 VL）

1. `band=16px` 的 **worst_dev ≤ 3** —— 边框带（⊇ 门禁的 4px 口径）全白。
   为什么不用 4px：4px 只能证明「最外一圈白」，白框/白留边/背景色块贴着边缘都会漏判。
2. `band=48px` 的 **mean_dev ≤ 16** —— 「无明显背景色块」这一条。
   白框/白留边包裹彩色照片的图，16px 带全白但 48px 带均值会冲到 30~100，靠它挡住。

任一次量不到（宿主没 `detect.white` 能力 / 老网关 404）→ **不跳过**，照常处理，
并在 `evidence.inputWhiteProbe` 标 `unavailable` / `partial_no_bg_block_probe`（不假装测过）。

**阈值标定（语料 v1.1，10 张 `white_bg_fill`，原始数据 `reports/evidence/task/process_gate.json`）**

| 样本 | 16px worst_dev | 48px mean_dev | 判定 |
|---|---|---|---|
| 8 张「白底图贴到有色背景」派生图（label `in_bg_dev_max` = 34 / 43 / 145） | 34 ~ 146 | 26.5 ~ 132.4 | 不跳过（照常刷） |
| 2 张**原生白底图** wbf_09 / wbf_10 | 0 / 0 | 2.17 / 0.02 | **跳过** |
| 自己产出的 10 张白底图（二次施加） | 0 | 0 ~ 2.1 | **跳过**（旧行为会重刷，wbf_07 被刷掉 28.5% 像素） |
| 合成对照：白框 20px / 40px 包彩色照片 | 0 | 105.1 / 28.9 | 不跳过 ✅ |
| 合成对照：白框 60px 包彩色照片 | 0 | 0.0 | 跳过（**已知边界**：白框 ≥ 48px 时与「白底图」不可区分） |

真实数据的判据余量是 **34 vs 3**（10 倍以上），不是把阈值压到「等于不设门」。

### 为什么守卫② 排在守卫① 前面

已白底的图连掩码都不需要——先测白度就能直接返回，一次 u2net（12MP 约 3s）全省。
代价是一张已是白底的图不做 `no_subject` 判定（那张图本来也没有「要刷」的诉求）。

### 守卫③ 为什么只数 `alpha == 255`

- `alpha == 0` 的像素**本来就该**被换成目标底色，改得越多越对；
- `0 < alpha < 255` 的斜坡像素**本来就该**与背景色混合（羽化/抗锯齿的定义），
  把它们算成「改动」会让守卫在每一张正常图上误报（首版就是这么错的：1128/19200 误报）；
- `alpha == 255` 是「原色照搬」——它们被改只可能是真 bug。

这三条都有**反向验证**单测（`tests/test_guard.py`）：把判据关掉，正向用例必须变红，
否则「守卫存在」只是断言、不是证据。

## 4. 边缘是怎么来的（为什么长这样）

1. 宿主 `/detect/u2net` 回的掩码是 **320² 网格最近邻放大**（`engine-segment` 口径），
   6016px 宽的图上**一格 ≈ 19px** —— 原始掩码是台阶，不是曲线。
2. 本包把二值掩码按网格尺度降采样，再**双线性**升回全分辨率 → 台阶变成一格宽的线性斜坡
   （这就是「抗锯齿」）。
3. `feather` 把降采样格放大 `k + feather` 再升采样 → 斜坡再宽 `feather` px。

**已知边界（写死，别当 bug 反复报）**：边缘精度上限 = 宿主掩码网格（≈ 图短边 / 320）。
要更细的边缘，得宿主把 `write_mask_png` 改成双线性升采样（协议面不变，属宿主能力增量），
不是本包能自己补出来的。

## 5. 为什么零第三方依赖（`requirements.txt` 只有注释）

- 宿主装包时只**建 venv**，不 `pip install`（`crates/app-web/src/compliance_api.rs::ensure_pack_venv`），
  所以任何 `import PIL/numpy` 的包在生产上都是「装好了但起不来」；
- 因此像素编解码自己写：`plugin/pngio.py`（8bit RGB/RGBA，五种 filter 全支持；
  编码固定 filter=None + zlib level 6 → 同输入同字节），合成在 `plugin/composite.py`。
- 原图解码、u2net 推理这类重活一律走宿主网关，包不重复实现（协议 §6 的 12 端点）。
- 守卫② 用到宿主的 `detect.white` 能力（`POST /detect/white_stats`）——**已有的**能力，
  不是新增端点；manifest 里如实声明，宿主按白名单授权（缺授权 = 守卫降级，不是报错）。

## 6. 性能（本机实测，原始数据 `reports/evidence/task/segment_e2e.log`）

只裁主体框那一块解码（不是全图），逐像素 Python 只出现在边缘斜坡段：

| 图 | 尺寸 | 单张耗时（含两次 u2net + 宿主编解码） |
|---|---|---|
| wbf_05 | 6016×4000（24MP） | 6.0 s |
| wbf_06 | 2831×3399 | 3.4 s |
| wbf_03 | 3504×2336 | 2.2 s |
| wbf_01 | 3221×2530 | 2.1 s |
| wbf_10 | 554×500 | 0.46 s |

10 张合计 **21.8 s**（平均 2.2 s/张）。慢的部分是纯 Python 的 PNG 反 filter
（逐字节）；要再快，正解是宿主补一个「alpha 合成」端点（协议面增量），
而不是往包里塞 Pillow（宿主装包不 pip install，塞了就是「装好了起不来」）。

## 7. 自检

```bash
bash packs/plugins/segment/tests/run_tests.sh        # 单测（含守卫反向验证）+ conformance 12 条 + venv 隔离
DAM_GATEWAY_E2E_MODELS=/models cargo test -p pack-host --test segment_pack_e2e -- --nocapture
```
