# 内置模型统一 fp16 转换 —— 效果对比测试 Plan

日期：2026-10-26
仓库：`/data/projects/soujpg-dam-desktop`
目标：把内置模型统一转 fp16，用真实语料对比精度与耗时，**按模型逐个裁决**是否替换，以减少安装包体积。不做一刀切。

---

## 0. 已核实的事实（本轮实测，非估算）

内置模型目录：`crates/app-tauri/resources/models/`（合计 1.11 GB，占 1.3G 安装包 85%）

| 模型 | 权重 dtype | 体积 | 用途 | 代码入口 | fp16 后可省 |
|---|---|---|---|---|---|
| siglip2-base text.onnx | **float16（已是 fp16）** | 538.5 MB | 文本塔 embedding | `engine-embed/src/onnx_siglip.rs` | **0** |
| siglip2-base vision.onnx | **float16（已是 fp16）** | 177.2 MB | 视觉塔 embedding | 同上 | **0** |
| siglip2 tokenizer.json | — | 32.8 MB | Gemma 词表 | 同上 | 0 |
| u2net.onnx | float32 | 167.8 MB | 抠图 matte | `engine-segment/src/lib.rs`（SIDE=320） | 83.8 MB |
| ppocrv6-medium-det | float32 | 59.2 MB | OCR 检测 | `engine-ocr/src/{infer,lib}.rs` | 29.6 MB |
| ppocrv6-medium-rec | float32 | 73.0 MB | OCR 识别 | 同上 | 36.5 MB |
| realesr-general-x4v3 | float32 | 4.6 MB | 超分 | `engine-restore/src/upscale.rs` | 2.3 MB |
| dinov3-vits16（external data） | float32 | 82.5 MB | 去重 embedding | `engine-embed/src/onnx_dinov3.rs` | 41.2 MB |

**结论一（先纠正预期）**：占 65% 体积的 siglip2 双塔**本来就是 fp16**（来自 onnx-community 官方 fp16 发布版）。fp16 路线的收益上限只有 ~193 MB，安装包 1.30G → ~1.11G（-15%）。要大幅瘦身必须走"siglip2 不内置/按需下载"或"词表裁剪"，见 §8 备选。

**结论二（本轮已跑出的实测基线，ORT CPU EP，10 张真实语料 `/data1/dam-models/corpus/nonproperty/*.jpg`）**

| 模型 | 体积 | 精度差异 | 耗时 fp32→fp16 | 判定倾向 |
|---|---|---|---|---|
| u2net | 167.8→84.0 MB | mask IoU 均值 0.9856（9/10 ≥0.994，car-01 因前景仅 ~500px 噪点 IoU 0.867，需改 dice + 面积过滤） | 169→226 ms（**1.33x 变慢**） | 需裁决（省 84MB 换 +57ms） |
| ppocr det | 59.2→29.6 MB | 概率图 corr ≥0.99998 | 387→281 ms（**0.73x 变快**） | 通过 |
| ppocr rec | 73.0→36.5 MB | 6 条噪声横条 1 条出现 [L] vs [U] 单字翻转（非有效文本样本） | 21→22 ms（1.06x） | 待有效文本语料复测 |
| dinov3 | 82.5→41.3 MB | 转换尚未跑通：ORT converter 在该图上产出**重名 Cast 节点**（`val_28_cast_to_fp32` 重复定义），需修图 | — | 待修图后测 |

**结论三**：fp16 在 ORT CPU EP 上**不是稳赚**——u2net 变慢 33%（fp16 算子覆盖差，插 Cast 往返），det 反而变快。所以本测试必须同时测精度与耗时，不能只看体积。

---

## 1. 测试设计总则

- **输入契约不变**：转换一律 `keep_io_types=True`，模型输入输出保持 float32。这样 `engine-segment` / `engine-ocr` / `engine-restore` 的 f32 张量构造代码**零改动**（siglip2 已有 `vision_in_f16` 自动探测，不涉及）。
- **每个模型独立裁决**，允许"部分替换"（例如 det 换 fp16、u2net 保留 fp32）。
- **语料**：全流程最多 10 张（沿用既定裁决）。分三类：
  - A 通用前景/物体：`/data1/dam-models/corpus/nonproperty/*.jpg`（10 张）
  - B 房源/商品（含文字）：`/data1/dam-models/corpus-mixed/property/set0*/`
  - C 文字专项：需 3~5 张确有文字的图（从 `/data1/dam-exif-corpus` 与 soujpg 现网带字图各挑，脚本跑 OCR 前后文本串取非空者）——**目标：非空文本 ≥5 条**，否则 OCR 结论无效（本轮 noise-bar 测试已证明空文本不可判）。
- **基线来源**：fp32 = 现网内置文件；fp16 = 本次转换产物。两侧同一 ORT、同一线程数（intra=8）、同进程交替跑，取 5 次均值。

---

## 2. 转换脚本（新增 `scripts/fp16-eval/convert.py`）

流程（每个模型一条）：

1. `onnx.load` → 节点名/value 名唯一化（重名加 `__dupN`）——**dinov3 必须做，其他模型做了也无害**。
2. `onnxruntime.transformers.float16.convert_float_to_float16(keep_io_types=True, op_block_list=[...])`
   - 通用 block：`Resize, Resize_11, Upsample, Range, CumSum, TopK, NonMaxSuppression`
   - u2net 有 Resize（上采样分支），不 block 会直接报 `input type float16 is invalid for Resize`
3. 转换后**三重校验**（缺一不可）：
   - `onnx.checker.check_model`
   - `onnxruntime.InferenceSession` 能加载（这一步能抓出重名 Cast / 类型不匹配）
   - 输出精度/形状与 fp32 一致
4. 产物落 `build/models-fp16/<model>/<file>`（不进 git，gitignore）。
5. 记录每条产物的 `sha256` + 字节数（后续更新 `model_store` manifest 用）。

已知坑（本轮踩过，直接写进脚本注释）：
- `disable_shape_infer=True` 会让 Resize 分支漏 block → 报 `input index 849 type`；
- dinov3：ORT 转换器对共享 value 插两次同名 Cast → 需转换后 value 重命名或对该分支加 `node_block_list`。若 30 分钟内修不好，**dinov3 直接判 keep fp32**（41MB 不值得卡主线）。

---

## 3. 精度评估（`scripts/fp16-eval/eval_*.py`）

| 模型 | 指标 | 通过阈值 | 备注 |
|---|---|---|---|
| u2net | 二值 mask **dice** + 面积过滤（前景 <1000px 的图不计入 IoU）+ alpha maxΔ | dice 均值 ≥0.998，单图 ≥0.995；alpha maxΔ ≤0.02 | 现有 IoU 在小前景上失真，必须换 dice |
| ppocr det | 概率图 corr + 阈值 0.3 前景像素数差 + 下游框数 | corr ≥0.9999，框数完全一致 | 用真实含字图 |
| ppocr rec | **CTC 解码后的整串 exact match** + CER | 非空文本样本 exact 100%，CER=0 | 语料 C 至少 5 条非空 |
| dinov3 | 10 图两两 cos 矩阵偏差 + top-1 邻居一致率 | 单对 cos ≥0.9995，矩阵偏差 ≤1e-3，top-1 100% | 去重场景对排序更敏感 |
| realesr | PSNR/SSIM（fp32 输出为基准） | PSNR 差 ≤0.1dB，SSIM ≥0.9995 | 4.6MB，收益小，最后做 |
| siglip2 | 不转换，仅确认现状 | — | 已是 fp16 |

评估落盘：`build/fp16-eval/report.json` + 一张 markdown 汇总表（每模型 × 指标 × 通过/不通过）。

---

## 4. 性能评估（`scripts/fp16-eval/bench.py` + 现有 Rust probe 交叉验证）

1. Python 侧：冷启动加载时长、单次推理（5 次均值/中位数）、进程 RSS 峰值。
2. Rust 侧交叉验证（跑真链路，防 Python/ORT 版本差）：
   - `cargo run -p dev-e2e --bin image_sim_probe`（embedding 耗时）
   - `cargo run -p dev-e2e --bin ocr_e2e`（OCR 全链路）
   - `cargo run -p dev-e2e --example upscale_bench`（超分）
   - 抠图无现成 bench → 用 `svc-pipeline` 现有 segment 单测路径人工计时，或临时加 `dev-e2e` 小 bin。
3. **通过阈值（绝对增量优先，比值次要）**：单次耗时 ≤ fp32 × 1.15 **且** 绝对增量 ≤150 ms。u2net 实测 1.33x / +57 ms → 比值不达标、绝对增量达标，进入人工裁决（§6）。

---

## 5. 体积核算与打包验证

1. 汇总替换后模型总大小，算出安装包预估：`1.30G - 193MB ≈ 1.11G`。
2. 替换 `crates/app-tauri/resources/models/` 对应文件（先备份原 fp32 到 `build/models-fp32-backup/`）。
3. `tauri.conf.json` 的 resources 映射不变（同名同路径替换），**无需改 tauri.conf**。
4. 若启用"按需下载"分支才需改 `engine-embed/src/model_store.rs` 的 `sha256` 清单——本 plan 的官方路径（内置替换）**不改 manifest**。
5. 实打一次 Windows 安装包（`souJpg-DAM` setup.exe），记录实际体积（对比 `releases/` 里 1.277G/1.296G 基线）。**验收以 setup.exe 实测字节数为准，不接受预估。**

---

## 6. 裁决矩阵

| 模型 | 替换条件（全满足） | 否则 |
|---|---|---|
| ppocr det | 精度通过 + 不慢于 1.15x | keep fp32 |
| ppocr rec | 非空文本 exact 100% + 不慢于 1.15x | keep fp32 |
| u2net | dice 通过 **且** 人工确认交互可接受（+57ms，抠图预览仍在 500ms 内） | 若判定慢得难忍 → keep fp32，放弃 84MB |
| dinov3 | 修图成功 + cos/top-1 通过 + 不慢于 1.15x | keep fp32（41MB 不阻塞） |
| realesr | PSNR/SSIM 通过 | keep fp32 |

预期落地结果（乐观）：安装包 1.30G → **1.11G（-15%）**；保守（u2net/dinov3 不换）：→ **1.19G（-9%）**。

---

## 7. 落地步骤（裁决通过后）

1. 用 fp16 产物覆盖 `crates/app-tauri/resources/models/<model>/`，`build/models-fp32-backup/` 留档。
2. 若是按需下载模型（dinov3/u2net 走 model_store 的情况）→ 更新 `model_store.rs` 的 sha256 与体积注释。
3. 回归：`cargo test -p engine-segment -p engine-ocr -p engine-restore -p engine-embed`；`cargo run -p dev-e2e --bin verify_all`。
4. 打包（真机安装包 + 体积记录）→ 真机冒烟：上传一张图走 抠图/OCR/去重/超分 四条链路，导出产物目视。
5. 报告：`build/fp16-eval/report.md`（体积前后、各指标、裁决结论、回滚路径）。

---

## 8. 备选（不在本 plan 执行，供决策）

1. **siglip2 按需下载**：安装包 1.30G → **~0.36G**（-72%），收益远超 fp16 路线，代价是首启联网拉 749MB（`model_store` 首启静默下载机制已存在）。
2. **siglip2 文本塔词表裁剪**：538MB 里 Gemma 256k 词表 embedding 是主因；裁到 32k 可省 ~400MB，但需重跑中文覆盖评估——工作量远大于 fp16。
3. **u2net int8 量化**（QDQ）：167.8 → ~42MB，比 fp16 再省一半，代价是精度掉得比 fp16 多，需单独评估。

---

## 9. 风险

- **CPU EP fp16 无硬件加速**：老 CPU（无 AVX512-FP16）上 fp16 走 Cast 往返，可能整体变慢（u2net 已证）。必须实测，不能只看文件变小。
- **Windows 目标机差异**：本机 Linux 测得比值，Windows 上 MLAS 路径可能不同 → 结论需在真机安装包上抽测一次耗时。
- **OCR 单字翻转**：rec 对量化敏感，exact match 门槛必须 100%，宁可不换。
- **回归面**：替换模型文件后 `sha256` 若与任何 manifest/测试硬编码断言不一致会挂单测（先 `grep -rn "sha256" crates/*/src` 确认）。
- **回滚**：保留 fp32 原件 + 打包脚本参数开关（`--models=fp32|fp16`），回滚 = 换目录重打。

---

## 10. 工期与拆卡

1. 卡1：转换脚本 + 三重校验 + dinov3 修图（半天）
2. 卡2：精度评估脚本 + 语料 C 准备 + report.json（半天）
3. 卡3：性能评估（Python + Rust 交叉）+ 裁决矩阵填表（半天）
4. 卡4：落地替换 + 安装包实测体积 + 真机冒烟（半天）

四张卡串行，卡4 依赖 1-3 全绿。
