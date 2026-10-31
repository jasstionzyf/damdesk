# 官方超分包（`com.soujpg.pack.upscale`）

一句话：**把图放大 2× 或 4×**，产物是资产的 PNG 工作版本（无损）。
`kinds: ["process"]` —— 只产出「处理结果 + 产物」，**没有**审核结论（不伪造 pass）。

## 1. 它解决什么

| 场景 | 用哪档 |
|---|---|
| 供应商给的图太小，要按平台要求放大后再上架 | `engine=full`（Real-ESRGAN，细节重建） |
| 只要尺寸够、耗时越短越好（批量缩略图/预览） | `engine=light`（宿主重采样，毫秒级） |
| 原图噪点多（手机夜景、压缩痕迹） | `engine=full` + `denoise>0`（模型自带降噪） |
| 大图（>4MP）也想走模型 | `engine=full` + `tile`（分块推理，见 §4） |

## 2. 参数（`resources/params.schema.json`，宿主 `ParamForm` 通用渲染）

| 参数 | 类型 | 取值 | 默认 | 说明 |
|---|---|---|---|---|
| `scale` | enum | `2` / `4` | `2` | 放大倍数。模型只有 4× 权重；2× = 「4× 模型 + `scale=0.5` 重采样」 |
| `engine` | enum | `full` / `light` | `full` | `full`=Real-ESRGAN（约 2~5 秒/张，4.7MB 模型）；`light`=重采样（毫秒级） |
| `denoise` | number | 0..1 | 0 | >0 强制走模型档：x4v3 自带降噪，**强度不可调**（evidence 记 `denoiseApplied=x4v3-builtin`） |
| `tile` | enum | `auto`/`384`/`768`/`1536` | `auto` | 分块边长。`auto` = 只在超过模型护栏 4MP 时按 1536 分块 |

两条**显式拒绝**（不做静默忽略，错误码 `-32003`）：

- `engine=light` 配 `tile≠auto`：重采样档没有模型输入护栏，分块没有意义；
- `engine=light` 配 `denoise>0`：重采样档没有降噪通道。要降噪就得用模型。

## 3. 产物与结果行

产物落在宿主注入的 `options.workdir`，文件名 `<assetId>-upscale-<倍数>x-<引擎>.png`；
中间件（4× 中间图 / 分块裁剪图 / 拼接画布）用完即删 —— 一个资产一次一个文件，不自建目录树。
结果行按协议 §4.4（处理类）：

```jsonc
{"assetId": "...", "result": "ok",
 "artifacts": [{"kind": "export_file", "path": "...", "sha256": "..."}],
 "evidence": {"engine": "full", "scale": 2, "inputWidth": 600, "inputHeight": 450,
              "expectedWidth": 1200, "echoed": "…", "outWidth": 1200, "outHeight": 900,
              "sizeExact": true, "tileBlock": null, "waitMs": 0, "elapsedMs": 2874,
              "startedAtMs": 1792684291632, "steps": [{"op": "upscale.realesr-x4v3", "scale": 4},
                                                     {"op": "upscale.resample", "scale": 0.5}],
              "sha256": "…"}}
```

失败分两类，都落 `result=failed` 行（`error{code,message}` 必带），**不炸整批**：

| 情形 | code | 消息里给什么 |
|---|---|---|
| 模型未装/未编入 local-infer（网关回了重采样） | `-32001` | 实际 `op` 值 + 三个原因 + 三条处置建议 |
| 目标尺寸超产物上限（像素 >1e8 / 长边 >20000） | `-32003` | 上限值 + 该源图的 `maxScale` |
| 网关不可达 / 403 能力未授权 | 网关错误码 | `host gateway 调用失败：<error>(<status>)` |
| 内存不足（画布/块解码） | `-32000` | 已做几步 + 「改小 tile / 降低 scale / 分批」建议，`evidence.resource.kind=memory` |
| 磁盘写满（`ENOSPC`） | `-32000` | 写不进的目录 + 「清理工作版本目录或换盘」，`evidence.resource.kind=disk` |
| 宿主取消 | `-32004` | 阶段名（对整批请求上抛，不落行） |

错误码只用协议 §5 的**闭集**（`-32001/02/03/04/05`、`-32000` 兜底、`-32601`、`-32700`）：
不在表里的 code 算协议违例，所以「未捕获异常」的兜底是 `-32000` 而不是 JSON-RPC 的 `-32603`。
超时不在包内计时：宿主按 `manifest.timeout_secs` 计时 → 发 `cancel` → kill，结果记
`plugin_timeout`；包的义务是逐块检查取消位并以 `-32004` 收尾。

## 4. 分块是怎么拼的（为什么不是硬切）

模型有**单次输入护栏 4MP**（`engine-restore::upscale::MAX_INPUT_PIXELS`；4× 输出 64MP f32）。
超了网关会退回重采样 —— 那是**静默降质**，本包不接受：`tile=auto` 时自动改走分块。

分块口径（`plugin/tiling.py`）：

1. 按块边长切网格（无重叠、无遗漏）；
2. 每块向外扩 **8px 边距**再送模型（模型感受野吃边界）；
3. 只把**去掉边距的内区**按行拷回 4× 画布 —— 拼缝落在「用边距算出来的像素」上，
   不会落在被截断的块边界像素上；
4. 2× 时画布再走一次 `/op/upscale scale=0.5` 落到目标尺寸（编码口径归宿主），
   4× 时画布即产物（包内 PNG 编码，filter=None + zlib 6 → 同输入同字节）。

实测：真模型下「分块 vs 整图」PSNR **64.47dB**（4 块，600×600 图），可视为同图。

**已知边界**：4× 画布 >1e8 像素时直接结构化失败（不把机器打死）；此时正解是先缩图。
内存峰值 ≈ 画布 300MB + 模型 4× 输出（与宿主同一份实现，包内不额外复制）。

## 5. 并发保护（CPU 档）

模型档单张 2~5 秒、峰值内存 ~700MB。同机并行跑 N 个会互相拖慢（实测 3 并发时单张
从 3s 涨到 11s），所以：

- `engine=full` 进**跨进程 flock 闸门**，默认 **1 槽**（`DAMPACK_UPSCALE_CPU_SLOTS` 可调）；
- 排队期间每 2 秒发 `progress{step:"waiting"}` → 任务中心显示「等待中」，不是卡死；
- 排了多久写进 `evidence.waitMs`；`engine=light` 不进闸门（毫秒级，卡它没收益）。

实测 3 个任务同时提交：`[0, 4437] / [4570, 8841] / 8841+` 首尾相接、无重叠，
6 条 `waiting` 通知（见 `reports/evidence/task/concurrency.json`）。

## 6. 质量口径（语料 v1 `process/upscale` 10 张实测）

| 图 | 输入 | label 期望 | 产物 | PSNR light | PSNR full |
|---|---|---|---|---|---|
| up_01 | 258×600 | 516×1200 | 516×1200 | 23.49 | 20.77 |
| up_02 | 600×450 | 1200×900 | 1200×900 | 30.25 | 27.82 |
| up_03 | 600×471 | 1200×943 | 1200×942 | 31.80 | 27.94 |
| up_04 | 600×600 | 1200×1200 | 1200×1200 | 28.43 | 29.14 |
| up_05 | 600×400 | 1200×800 | 1200×800 | 29.59 | 29.64 |
| up_06 | 600×460 | 1200×921 | 1200×920 | 38.23 | 32.87 |
| up_07 | 600×399 | 1200×798 | 1200×798 | 32.01 | 31.02 |
| up_08 | 499×600 | 999×1200 | 998×1200 | 34.59 | 29.44 |
| up_09 | 400×600 | 800×1200 | 800×1200 | 30.86 | 31.17 |
| up_10 | 600×400 | 1200×800 | 1200×800 | 36.50 | 34.35 |

- **尺寸**：10/10 严格 = 输入×倍数；其中 7/10 与 label `out_size` 严格相等。另 3 张
  （up_03/06/08）label 的期望是**奇数边原图**，语料派生时 `floor(odd/2)` 掉了 1px，
  2× 放大在数学上取不回来 —— 任何 2× 超分都不可能等于那个数。
- **PSNR（对 `_ref`）**：light 9/10 ≥28dB，full 7/10 ≥28dB（门槛达成情况逐图见上）。
  full 档在这批「½ 派生」语料上 PSNR 并不占优：Real-ESRGAN 会**重建细节**，而 GT 是
  降采样得到的、本身没有那些细节 —— PSNR 惩罚的正是模型的「凭空长出来的高频」。
  真实使用（放大真小图）时 full 才有意义；要「最像原图」就选 light。

## 7. 为什么零第三方依赖

宿主装包只建 venv、**不 pip install**（`ensure_pack_venv`），所以 `import numpy/onnxruntime`
的包在纯净机器上是「装好了永远起不来」。因此：推理与几何（裁剪/缩放/编码）全走宿主网关，
包内只有最小 PNG 编解码（`plugin/pngio.py`）与分块拼接（`plugin/tiling.py`）：
不是算法，只是 bytes 行拷贝。

## 8. 自检

```bash
bash packs/plugins/upscale/tests/run_tests.sh          # 单测 58 + conformance 12 + 隔离解释器
bash packs/plugins/upscale/tests/run_tests.sh --e2e    # 追加真网关真模型语料 e2e（需 /models + 语料）
```

独立起一个真网关做手工调试（不改协议、不改库 API）：

```bash
DAM_MODELS_ROOT=/models cargo run -q -p pack-host --example gateway_serve -- \
  --work-dir /tmp/gw --capabilities io.read,op.geometric
# stdout 一行：{"url":"http://127.0.0.1:PORT","token":"..."}
```
