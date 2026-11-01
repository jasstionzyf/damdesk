# DAMDesk 开源发布 plan（GitHub 首发）

> 状态：draft（2026-10-31）｜作者：主会话｜配套：`docs/ARCHITECTURE.md`（待写）、`docs/industry-pack-protocol.md`（已有 v1.1）
> 目标：把 DAMDesk 以「可安装处理插件 + 本地优先 DAM」的定位首发到 GitHub，叙事核心是**插件机制**，技术文档只保留核心架构。
> 数据来源：GitHub REST API 实查（2026-10-31，原始 JSON `~/.hermes/profiles/soujpg/cache/scratch/gh-competitors.json`）+ 仓内 `docs/`、`packs/`、`tools/`、`crates/`。

---

## 0. 结论先行（6 条）

1. **赛道热度有、插件空白真存在**：图库/DAM 头部（immich 11.8 万 star、PhotoPrism 4.0 万、Ente 3.0 万）都已稳定发版，但**只有 immich 一家有第三方代码插件**，且锁在 WASM 沙箱内（Extism），装插件还要改环境变量+重启，SDK 未发 npm。
2. **可安装「处理类」插件是没人占的位置**：PhotoPrism 的插件请求 issue #117 自 2019-06 挂到现在（open，7 年），#5316（2025-11）再提一次仍未做；其余 DAM 全为单体或配置式扩展。DAMDesk 的「子进程插件 + 能力网关 + `.dampack` 装机/切版本/回滚 + conformance 可执行规格」目前无直接对标物。
3. **首发不拼 star，拼叙事**：竞品体量差 1~2 个数量级，首发目标应是「第一个把行业合规流程做成可安装插件的开源 DAM」，用 3 分钟可复现的插件 demo 换关注，而不是功能清单对轰。
4. **发布形态：新仓 squash 首发，不分叉历史**。现仓 622 commits / 121 MB pack / 3233 文件，其中 `reports/` 2138 个文件（内部证据截图、机器路径、测试语料），684 个文件含 `soujpg`，121 个含内网 IP `172.25.45`，112 个含 `gpu7`。历史清理成本 > 重建成本，且内部 Gitea 继续做开发主线。
5. **两处硬合规必须先解决，否则不能发**：① 判重塔 DINOv3 权重是 **Meta gated 自定义许可**（GitHub `facebookresearch/dinov3` license=NOASSERTION，HF 需申请接受协议）→ **不得随安装包/GitHub Release 再分发**，改为用户自备或按需下载；② 品牌面 `soujpg` 与 `com.soujpg.*` 标识需定口径（推荐：对外品牌 damdesk，identifier/pack id 首版保留并文档说明，避免 appData 迁移）。
6. **文档只出 6 份**：README、ARCHITECTURE、PLUGIN-PROTOCOL（已有，扩一节快速开始）、CONTRIBUTING、LICENSE+NOTICE、SECURITY。不写部署手册、不写全量 API 参考、不搬内部 PLAN.md 与 reports。

---

## 1. 竞品 GitHub 现状（实查 2026-10-31；**仅内部参考，不进任何对外内容**）

### 1.1 基础数据

| 项目 | Stars | 最后 push | License(SPDX) | 最新 release | 类型 |
|---|---:|---|---|---|---|
| immich-app/immich | 117,748 | 2026-10-30 | AGPL-3.0 | v3.3.5 | 照片/视频库（self-hosted） |
| photoprism/photoprism | 40,378 | 2026-10-29 | NOASSERTION（自有许可） | 261029-963b58c50 | 照片库 + AI 索引 |
| xinntao/Real-ESRGAN | 37,286 | 2024-08-06（停更） | BSD-3-Clause | v0.3.0 | 超分工具库 |
| ente-io/ente | 29,778 | 2026-10-30 | AGPL-3.0 | photos-v1.3.67 | 加密相册（端云） |
| HumanSignal/label-studio | 28,592 | 2026-10-30 | Apache-2.0 | 1.23.2 | 标注平台 |
| danielgatis/rembg | 25,244 | 2026-10-21 | MIT | v2.0.86 | 抠图库 |
| Sanster/IOPaint | 23,296 | 2025-04-29 | Apache-2.0 | iopaint-1.5.3 | 局部重绘 |
| LibrePhotos/librephotos | 8,121 | 2026-10-30 | MIT | 1.5.0 | 照片库 |
| Photoview/Photoview | 6,564 | 2026-10-29 | AGPL-3.0 | v2.4.0 | 照片库 |
| chaiNNer-org/chaiNNer | 6,120 | 2026-10-27 | GPL-3.0 | v0.26.1 | 图像处理节点链 |
| lycheeorg/Lychee | 4,323 | 2026-10-29 | MIT | v7.12.0 | 相册 |
| pimcore/pimcore | 3,885 | 2026-10-30 | NOASSERTION（GPLv3+商业） | v2026.3.3.1 | 企业 PIM/DAM |
| Piwigo/Piwigo | 3,875 | 2026-10-29 | GPL-2.0 | 16.4.0 | 相册（PHP） |
| rom1504/clip-retrieval | 2,798 | 2026-03-28 | MIT | 2.45.0 | 图文检索基建 |
| Webreaper/Damselfly | 1,796 | 2026-09-29 | GPL-3.0 | 4.5.3 | server 型 DAM |
| nuxeo/nuxeo | 709 | 2026-10-30 | Apache-2.0 | — | 企业内容平台（Java） |
| razuna/razuna | 179 | 2026-10-29 | AGPL-3.0 | — | 老牌开源 DAM |
| resourcespace/resourcespace | 88 | 2026-10-30 | NOASSERTION | — | 企业 DAM（PHP，主仓在自有 Git） |
| KDE/digikam（GitHub 镜像） | 807 | 2019-12-21（镜像停更） | GPL-2.0 | — | 桌面照片管理 |

### 1.2 插件/可定制化机制矩阵（本 plan 的重点）

| 项目 | 第三方代码可扩展？ | 形态 | 能跑原生库/私有模型？ | 装机/版本/回滚 | 可执行一致性验证 |
|---|---|---|---|---|---|
| **DAMDesk** | 是（首发即有） | 独立子进程 + stdio 行式 JSON-RPC 2.0；manifest 是唯一配置面 | **能**（包内 venv，任意 Python 依赖，私有算子/本地模型） | 有（`.dampack` install/activate/rollback/uninstall，签名可选） | 有（`tools/pack-conformance` 14 断言（01–12 协议 + 13/14 i18n）+ 假网关 + 违规变体） |
| immich | 是（2026 新增） | WASM/Extism 插件 + 事件驱动 workflow；`hostFunctions`/`allowedHosts` 权限清单 | 不能（沙箱内，只能调宿主 host functions） | 弱（API 只读；装插件=放文件+环境变量+**重启**；无更新/发现） | 无公开规格套件；SDK 未发 npm（workspace only） |
| photoprism | **否** | — | — | — | — |
| LibrePhotos / Photoview / Ente / Lychee / Damselfly | 否（无公开插件规范） | — | — | — | — |
| Piwigo | 是 | PHP 插件（drop-in 目录） | 能（同栈 PHP） | 手工（上传/启用） | 无 |
| ResourceSpace | 是 | PHP 插件（`plugins/` 目录 drop-in） | 能（同栈 PHP） | 手工 | 无 |
| Pimcore | 是 | Symfony bundle / DataHub | 能（同栈 PHP） | Composer | 无 |
| Nuxeo | 是 | Java 扩展点/插件（平台级） | 能（同栈 JVM） | 部署式 | 无 |
| digiKam | 是 | KIPI 插件 + Batch Queue Manager | 能（C++/Qt） | 包管理器 | 无 |
| ComfyUI / chaiNNer | 是 | 节点图（custom nodes） | 能 | 手工/管理器 | 无 |
| Label Studio | 是（ML backend） | 独立 HTTP 服务 | 能 | 手工 | 无 |

关键事实（可复核）：
- immich 仓内已有 `packages/plugin-sdk`、`packages/plugin-core`、`server/src/controllers/plugin.controller.ts`、插件/工作流三张表；插件安装目前是「`IMMICH_ALLOW_EXTERNAL_PLUGINS=true` + 放文件 + 重启」，API 只读（Discussion #29998，2026-07-17）。
- PhotoPrism 插件请求 issue #117：**2019-06-06 创建，至今 open**；#5316（2025-11-11）仍是 open。
- 数据局限：Piwigo/ResourceSpace/Pimcore/Nuxeo/digiKam 的插件机制为**官方文档级**结论，未逐文件核实；`reports` 里的 star 数为 API 快照，会漂移。

### 1.3 三条判断

1. **「能装第三方代码」不稀缺，「能装会处理图像/跑私有模型的第三方代码」稀缺**：WASM 沙箱天然跑不了 vips/onnx/私有 C 扩展；同栈插件（PHP/Java/C++）要求用户接受整套技术栈。DAMDesk 的子进程模型是唯一「不锁语言、不锁依赖、宿主零改动」的路径。
2. **装机体验是分水岭**：immich 的插件实际处于「dev-only」状态（自己 RFC 里承认）。谁先把「装/切版本/回滚/验签/一致性套件」做完整，谁拿到生态位。
3. **确定性与可审计是 B 端话术**：竞品插件无结果契约、无 `undetermined` 三态、无 `packId@version#hash12` 结果身份。行业合规场景下这是硬需求，也是我们区别于「AI 玩具」的地方。

---

## 2. 对外定位与差异化（README/ARCHITECTURE 的唯一主线）

一句话：**本地优先的桌面 DAM，行业处理流程是可安装、可回滚、可验证的插件，不是写死在代码里。**

五条硬差异（写进 README 顶部三屏内，每条一句 + 一个可复现链接）：
1. **插件是独立子进程**：任何语言、任意依赖、私有模型都能进；宿主只给能力网关（IO/几何/分割/OCR/VL/生成式/报告 15 端点，白名单鉴权）。
2. **宿主不解释流程**：判定与修复的步数/顺序/分支全在插件代码里，不发明 YAML/DSL（配置表达力不够，行业间是结构差异）。
3. **装/切/回有据**：`.dampack` 归档，manifest 原文 sha256 = 结果身份的一部分（`packId@version#hash12`），内置包受保护（403）、当前版本不可卸（409）。
4. **一致性是可执行的**：`tools/pack-conformance` 14 条断言（01–12 协议面 + 13/14 i18n）+ 假网关 + 参考实现 + 4 个违规变体；新插件「写对」不靠读文档靠跑套件。
5. **判定可审计**：`pass/fail/undetermined` 三态，超时/崩溃绝不落 pass 也不落 fail；生成式只能局部可选，默认 `allow_redraw_subject=false`。

三条诚实边界（写进 ARCHITECTURE「不做什么」，避免被当能力缺口打）：
- 不做插件市场/在线分发、不做热重载、不做 WASM 沙箱（沙箱跑不了真实算子；信任靠签名+权限+workdir 边界）。
- 内置核心（打标/去重/EXIF/OCR/向量检索）永远内置，不包成插件。
- 没有账号/团队/审批流（这些是云端钩子）。

竞品**不进对外内容**（用户裁决 2026-10-31）：README/官网/渠道文案一律不点名竞品、不做对比表。
竞品调研只用于两件事：① 判断生态位（§1.3）；② 学它们的仓库与文档组织方式——README 三段式、`docs/` 按主题单文件、`.github/` 模板齐全、Release note 固定结构（已写进 GH-2/GH-8 卡体）。

---

## 3. 发布形态与仓库边界（决策）

| 项 | 决策（推荐） | 理由 |
|---|---|---|
| 历史 | **新仓 squash 单提交**，不带 622 commits | 121 MB pack、`reports/` 2138 文件、内网 IP/机器路径遍布；`filter-repo` 后仍留语料版权与路径残留 |
| 开发主线 | 内网 Gitea `soujpg-dam-desktop` 不变；GitHub 为**单向发布目标** | 不打断现有 kanban 链与真机流水线 |
| 同步 | 里程碑式手动 mirror（`publish_mirror.sh`：白名单目录 + 敏感扫描 + 体积门禁），不接受直接向 GitHub main 推 | 避免两份历史分叉；PR 走 cherry-pick 回内网 |
| 组织/仓名 | `damdesk` 用户名/组织**已被占用**（GitHub 存在用户 Damdesk）；候选可用：`damdesk-app` / `getdamdesk` / `damdesk-labs` / `opendamdesk`（均 404 可用，已探）。推荐 org=`damdesk-labs`，仓名 `damdesk` | 仓名短、org 承载品牌；首次即建 org 便于后续插件仓/SDK 仓 |
| 许可 | 代码 **AGPL-3.0**（与竞品头部一致，防闭源 SaaS 白嫖）；商业双许可保留 | immich/Photoview/Ente 都 AGPL；Pimcore 双许可先例 |
| 版本 | 首发 `v0.1.0-preview`（诚实标 preview） | 单测/真机已过，但 API 与插件协议仍会动 |
| 二进制 | Windows NSIS + macOS dmg ≈ 940 MB，资产 = 程序 190 MB + **内置 siglip2 双塔与 tokenizer 749 MB**；另挂 `damdesk-models-extras-v1.zip`（504 MB：u2net/ppocr/realesr/lama）+ `SHA256SUMS` | 检索是基础功能，删掉开箱即残；siglip2 已是 fp16，量化无收益（上限 193 MB） |
| 模型获取 | ① 内置：siglip2（检索/相似/以图搜图开箱可用，离线可用）；② 按需：4 个增强模型应用内一键下载（504 MB；主源 HF `damdesk/models`、镜像腾讯 COS ap-shanghai，失败自动切源，也可改 `file://`）；③ 自备：DINOv3（Meta gated）放 `models/dinov3-vits16-onnx/`，缺失时判重 `undetermined` 且 UI 明示 | 现 `DOWNLOADABLE` 只有 lama-fp32，需 GH-9 接线；内置/按需边界见 GH-5 卡体表 |
| 容器 | `ghcr.io/<org>/damdesk:<tag>`（app-web 容器形态，无 ONNX） | 容器形态二进制仅 8.8 MB，镜像好发 |
| 模型 | SigLIP2 / u2net / Real-ESRGAN / PP-OCRv6 / LaMa 走公开下载源；**DINOv3 移除**，判重塔降级为「可选，用户自备」并在 UI 明示 | 合规硬约束（见 §6） |

---

## 4. 操作序列（P0–P7，命令级）

### P0 决策与账号（0.5 天）
1. 裁决三件事：① 品牌与 `soujpg` 的关系（推荐 expose=damdesk，identifier/pack id 首版保留）；② DINOv3 是否按推荐移除；③ 首发是否出二进制（推荐出，否则插件机制没法被试用）。
2. 建 GitHub org（推荐 `damdesk-labs`）+ 空仓 `damdesk`（private 先建），开启 2FA、org 级 secret scanning、Dependabot。
3. 建标签：`plugin`、`dam`、`tauri`、`rust`、`local-first`、`compliance`、`industry-pack`；repo description 用一句定位（英文）。

验收：`gh repo view <org>/damdesk` 可读；org 名与仓名在 GitHub 搜索里无同名冲突。

### P1 公开仓骨架（1.5 天）
```bash
# 1) 从内网主线拉一份干净副本（不带历史）
cd /data/projects && git clone --no-hardlinks soujpg-dam-desktop damdesk-public
cd damdesk-public && rm -rf .git && git init -b main

# 2) 删内部资产（先打包备份到 /data1/damdesk-internal-backup/<date>/）
rm -rf reports .hermes data registry dev
# scripts/ 280 个：只保留被 CI/测试引用的白名单，其余移 internal/
#   保留：test_gate.sh pack_lint.sh k15_build_packs.py merge_acceptance.sh
#         builtin_guard.sh check_layers.sh sample_real_photos.sh
# 3) 加对外文件（见 §5）与 .github/
```
必做清理（每条都给验收命令）：
```bash
# a) 商业/内部词：文档与 UI 文案零命中（代码标识符按 §6 白名单豁免）
git grep -nE 'soujpg|SOUJPG' -- ':!docs/' ':!*.lock' | wc -l      # 目标 0
# b) 内网地址/主机名
git grep -nE '172\.25\.45|172\.17\.0|gpu[0-9]\.dev|host\.docker\.internal' -- ':!*.md' | wc -l  # 目标 0（文档里改 env 占位）
# c) 凭证
gitleaks detect --no-git -v    # 0 finding；.gitleaks.toml 白名单只放 com.soujpg.* 标识
# d) 体积
git ls-files -z | xargs -0 du -b | sort -rn | head -20   # 单文件 >20MB 需写明理由（当前仅 NotoSansSC 17.7MB）
```
验收：`cargo check --workspace` 通过；`web` 能 `pnpm build`；仓体积 <30 MB。

### P2 合规审计（1 天）
1. 依赖许可：`cargo deny check licenses`（或 `cargo about generate`）+ `pnpm licenses list`，产出 `NOTICE`/`THIRD-PARTY.md`。
   - 特别标注：**libvips = LGPL-2.1（动态链接）**，mac 包自带 dylib，需在 NOTICE 写「动态链接、可替换」；ORT = MIT；Tauri = MIT/Apache-2.0。
2. 模型许可表（**并入 `NOTICE`，不另开文件**）：
   | 模型 | 上游 | 许可 | 可否再分发 |
   |---|---|---|---|
   | SigLIP2 base（ONNX） | google/siglip2 | Apache-2.0（**需复核 HF 卡**） | 可 |
   | DINOv3 ViT-S/16（ONNX） | facebookresearch/dinov3 | Meta 自定义 + gated（NOASSERTION） | **不可** → 移除 |
   | u2net | xuebinqin/U-2-Net | Apache-2.0 | 可 |
   | Real-ESRGAN x4v3 | xinntao/Real-ESRGAN | BSD-3-Clause | 可 |
   | PP-OCRv6 det/rec | PaddlePaddle/PaddleOCR | Apache-2.0 | 可 |
   | LaMa | advimman/lama | Apache-2.0 | 可 |
   | Noto Sans SC | Google | OFL-1.1 | 可（保留版权行） |
3. 语料/截图：`reports/`、`tests/fixtures/reals/` 一律不进公开仓；官方包 README 里的样例图换自有无版权素材。
4. 商标：README 声明「DAMDesk 为项目名；代码不含任何商业计费/采购逻辑」，`soujpg` 仅在 identifier 说明段出现一次。

### P3 CI 与门禁（1.5 天）
`.github/workflows/ci.yml`（ubuntu-latest）：
```yaml
# 关键步骤（顺序即门禁）
- apt-get install -y libvips-dev pkg-config cmake
- cargo fmt --check && cargo clippy --workspace -- -D warnings
- cargo test -p core-storage -p engine-dedup -p engine-exif -p pack-host -p svc-import
- pnpm -C web install --frozen-lockfile && pnpm -C web build && pnpm -C web test
- bash tools/pack-conformance/run_all.sh          # 14 断言 + 4 违规变体，必须 exit 0
- bash scripts/pack_lint.sh packs/plugins/ecommerce
- node tools/pack-sdk-python smoke（SDK 3918 行，spawn→health <1s）
```
`.github/workflows/release.yml`（手动 `workflow_dispatch`）：Windows NSIS + macOS dmg 走**自托管 runner**（当前无 mac/win 云端 runner，skill 已记此未定项），产出 SHA256SUMS + Release note。
验收：PR 上 CI 全绿；release dry-run 能出包（可先只 Windows）。

### P4 Release v0.1.0-preview（1 天）
```bash
git tag -a v0.1.0-preview -m "DAMDesk preview: installable processing plugins"
# Release 附件：damdesk_0.1.0_x64-setup.exe / .dmg（无模型）+ SHA256SUMS + NOTICE
# 镜像：ghcr.io/damdesk-labs/damdesk:0.1.0-preview
```
Release note 结构固定：一段定位 → 插件机制 3 条 → 5 个官方包列表 → 已知限制（无市场/无热重载/DINOv3 自备）→ 获取模型的路径。

### P5 社区面（0.5 天）
- Issue 模板：bug / feature / plugin-request（后者要求贴 `manifest.json` + conformance 输出）。
- PR 模板：贴 CI 结果 + 是否改协议（改协议必须同步 conformance 与 `api_version`）。
- `CODE_OF_CONDUCT.md`（Contributor Covenant 2.1）、`SECURITY.md`（私密报告 + 48h 响应承诺）、Discussions 开 4 个分类。

### P6 冷启动（1 天，可与 P5 并行）
- 英文：HN Show HN（标题打「installable plugins」而非「AI DAM」）、Reddit r/selfhosted / r/DataHoarder、`awesome-selfhosted` PR、`awesome-dam` 类清单。
- 中文：V2EX、即刻、少数派。
- 内容资产：30 秒 GIF（装插件→跑判定→看报告）+ 3 分钟视频（写一个 20 行插件 → conformance → 装机 → 判定）。
- 每个渠道文案只讲一件事：**「行业流程写成插件，装上就能跑，回滚一条命令」**。

### P7 运营机制（长期）
- `scripts/publish_mirror.sh`（内网→公开，白名单 + 敏感扫描 + 体积门禁 + 生成 squash 提交），每次发布前跑。
- 公开仓收到 PR → 内网 cherry-pick → 下次 mirror 回流（在 CONTRIBUTING 写明，避免 contributor 期待直接合）。
- 每季度复跑 §1.1 竞品数据（脚本已存在：`gh_probe.py`），**只用于内部判断生态位与文档组织习惯，不产出任何对外对比内容**。

---

## 5. 对外文档集（只 6 份，明确不写什么）

| 文件 | 内容 | 长度上限 |
|---|---|---|
| `README.md` | 是什么/给谁/60 秒跑起来/插件机制三屏/构建与测试/文档索引/许可；**不写竞品对比** | ≤200 行 |
| `docs/ARCHITECTURE.md` | **唯一架构文档**：两形态一代码（桌面壳/容器）、crate 分层图、数据流（导入→打标→检索→判定→修复→导出）、存储（SQLite+sqlite-vec、appData 固定项）、推理四态、**插件运行时**（子进程模型、能力网关、结果契约三态）、取舍清单（为什么不是 DSL/wasm/LanceDB） | ≤400 行 |
| `docs/industry-pack-protocol.md`（沿用现有文件名，不改名，链接稳定） | 已有 v1.1 冻结稿，**只加一节「Quick start（20 行插件 + 一条 conformance 命令）」**，其余不动 | 现有 + 40 行 |
| `CONTRIBUTING.md` | 开发环境（dam-dev 容器一条命令）/门禁四连/提交规范/PR 流程（mirror 机制）/插件贡献路径 | ≤80 行 |
| `LICENSE` + `NOTICE` | AGPL-3.0 全文 + 第三方与模型许可表（含 libvips LGPL 动态链接声明、OFL 字体版权行） | 标准文本 + 1 页 |
| `SECURITY.md` | 漏洞报告渠道、范围、响应时限；插件信任模型（签名可选、workdir 边界、无网络默认） | ≤40 行 |

**不写/不搬**（避免文档膨胀）：
- 内部 `PLAN.md`（M0–M6 工程基准）、`.hermes/plans/`、kanban 卡体、`reports/`（2138 文件）。
- 全量 REST API 参考 → 由 axum 路由生成 OpenAPI 后放 `/docs/api`（自动，不手写）。
- 部署手册/运维 runbook（容器 compose 在 `deploy/` 有示例，够了）。
- 行业包业务细节（ecommerce/realestate 只留各自 `README.md` 的算法与阈值依据）。
- 商业化/计费/云端协作（明确属另一产品线，不进本仓）。

---

## 6. 合规与「soujpg 残留」口径

现状实测：`git ls-files` 中 684 个文件含 `soujpg`，其中代码侧集中在：
- `crates/app-tauri/tauri.conf.json` → `identifier: com.soujpg.dam.desktop`（决定 appData 路径：`%APPDATA%\com.soujpg.dam.desktop\`）
- `com.soujpg.pack.{ecommerce,realestate,imaging,...}` 插件 id（结果身份 `packId@version#hash12` 的一部分）
- 测试夹具、seed spec、UI 文案

两档处置（推荐 A）：
- **档 A（推荐，成本 0）**：identifier 与 pack id **不改**，在 ARCHITECTURE「命名与历史」段说明「反向域名沿用早期工程命名，属稳定标识，不代表商业关系」；公开面（README/UI/docs/包名/二进制名）全部用 damdesk。
- **档 B（成本 1–2 天 + 迁移）**：identifier 改 `app.damdesk.desktop`，需 appData 迁移脚本（老用户 db/thumbs/models 平移）+ 全部测试夹具与 migration 断言改号；pack id 改 `io.damdesk.pack.*` 会让**已发布包的结果身份失效**，不建议在首发前做。

无论哪档，硬门禁是：**README/docs/UI 文案/shell 输出 零 `soujpg`**；内网地址零残留。

---

## 7. Publish gate（一个脚本，发布前必跑）

`scripts/publish_gate.sh`（放内网，不进公开仓）：
```bash
set -e
# 白名单 = 档 A 保留的稳定标识（identifier + pack id + 文档说明段）；其余任何位置命中即失败
git grep -nE 'soujpg|172\.25\.45|172\.17\.0|gpu[0-9]\.dev|sk-[A-Za-z0-9]{20,}' \
  -- ':!crates/app-tauri/tauri.conf.json' ':!crates/core-config/src/packs.rs' \
     ':!packs/**/manifest.json' ':!docs/ARCHITECTURE.md' \
  | tee /tmp/publish-gate-leak.txt
[ ! -s /tmp/publish-gate-leak.txt ] || { echo "敏感词残留（见上）"; exit 1; }
gitleaks detect --no-git && true
[ $(git ls-files -z | xargs -0 du -b | sort -rn | head -1 | cut -f1) -lt 20971520 ] || { echo "存在 >20MB 文件"; exit 1; }
du -sh .git | awk '{print "repo:",$1}'
cargo check --workspace
pnpm -C web build
bash tools/pack-conformance/run_all.sh
bash scripts/pack_lint.sh packs/plugins/ecommerce
ls packs/plugins | wc -l   # 官方包数量 = 5
```
通过标准：全绿 + `git ls-files | wc -l` ≈ 1100（3233 − reports 2138 + 新增文档）。

---

## 8. 排期与拆卡建议（8 个工作日起）

| 阶段 | 内容 | 工时 | 卡拆分（soujpg 板） |
|---|---|---|---|
| P0 | 决策 + org/仓 + 标签 | 0.5d | 主会话直接做 |
| P1 | 公开仓骨架 + 清理 + 对外 6 份文档 | 2.5d | 4 张：骨架/清理、README、ARCHITECTURE、PROTOCOL+CONTRIBUTING+SECURITY |
| P2 | 许可审计 + DINOv3 移除 + NOTICE | 1d | 2 张：依赖与模型许可、DINOv3 移除与判重塔降级 |
| P3 | CI 两条 workflow + 首次全绿 | 1.5d | 2 张：ci.yml、release.yml（self-hosted） |
| P4 | v0.1.0-preview Release（包+镜像+note） | 1d | 2 张：构建产物、Release 物料 |
| P5/P6 | 社区面 + 冷启动内容 | 1.5d | 2 张：模板/CoC/Security、demo 资产与渠道文案 |
| P7 | publish_mirror.sh + 季度竞品复跑 | 0.5d | 1 张 |

卡号映射（soujpg 板，均 model=glm-5.3-flash / provider=zhipu / workspace=dir:/data/projects/damdesk-public）：
GH-1 `t_582c5728` 基线 → GH-2 `t_5a2388e2` README → GH-3 `t_47fca6d2` ARCHITECTURE → GH-4 `t_ef1650f2` 其余文档；
GH-1 → GH-5 `t_5b3404af` 许可/DINOv3、GH-6 `t_18723be0` CI；GH-9 `t_a148715b` 模型分发接线（父 GH-5）、GH-10 `t_f24329ac` 分发物料与上传（父 GH-5）；GH-7 `t_19438895` Release（父 GH-5+GH-6+GH-9+GH-10）；GH-8 `t_0a931d1e` 社区/gate（父 GH-4+GH-6）。

依赖：P2 的 DINOv3 决策阻塞 P4（打包）；P1 的 ARCHITECTURE 阻塞 README 的文档索引与插件章节定稿。CI（P3）可与 P2 并行。

---

## 9. 风险与回滚

| 风险 | 概率/影响 | 处置 |
|---|---|---|
| DINOv3 gated 权重被随包分发 | 高/法务硬伤 | P2 强制移除；打包脚本加「模型目录白名单」断言，出现在包里即构建失败 |
| `soujpg` 品牌暴露引发定位混淆 | 中/中 | 档 A 只有 identifier 与 pack id 残留，文档一段说明；发布前 `git grep` 零命中门禁 |
| 公开仓与内网主线分叉 | 中/中 | 只允许 mirror 单向；PR 走 cherry-pick，CONTRIBUTING 写明 |
| mac 构建依赖私有 dylib（VIPSHOME 方案） | 中/高 | P3 在干净 mac 上验一次「无本地依赖能否构建」；不行就先只发 Windows + 源码（诚实写在 Release note） |
| 首发无人关注 | 高/低 | 叙事聚焦插件机制 + 可复现 demo；不承诺 roadmap |
| AGPL 吓退企业用户 | 中/中 | README 说明「自用/内部部署无额外义务」，FAQ 一段；双许可按需谈 |
| 插件安全质疑（跑任意代码） | 中/中 | ARCHITECTURE 写清信任模型：签名可选 + 权限 manifest + workdir 边界 + 默认禁网；不吹沙箱 |

回滚：发布前一切在 `damdesk-public` 独立目录，删目录即回滚；GitHub 仓首发可 private→public 二次确认；tag 可删重打（首发 72h 内）。

---

## 10. 明确不做

- 不做插件市场/在线分发/热重载（协议 v1.1 已裁决，不因发布改口）。
- 不把内置核心（打标/去重/EXIF/OCR/向量检索）包成插件。
- 不搬内部 reports/kanban/PLAN.md 上 GitHub。
- 不在首发做云同步/团队/审批流（PLG 钩子，属另一产品）。
- 不为「文档齐全」写部署手册与全量 API 文档（自动生成优先）。
