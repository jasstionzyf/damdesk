# 2026-10-22 插件体系重规划：按新设计改写 kanban 任务

> 权威设计：`damdesk-product/references/plugin-design-and-ui.md`（总纲）、`plugin-result-contract.md`、`plugin-param-schema.md`。
> 本文只回答一件事：**现有 kanban 卡按新设计怎么处置，缺什么要新建**。
> 当前状态：damdesk kanban 已暂停（dispatcher 已停、running 卡已 block），恢复时按 §5 执行。

---

## 1. 新设计与既有卡的差异（差异点即返工点）

| # | 旧口径（既有卡按此实现） | 新口径（2026-10-22） | 影响 |
|---|---|---|---|
| D1 | 「行业包」= 独立实体（四件 YAML），插件是执行代码 | 三档：**内置功能 / 插件 / 规格**；行业包概念作废 | 删 YAML 执行路径；新增「规格」实体 |
| D2 | 规格不存在；平台差异靠"装不同行业包" | **规格 = 选规则 + 阈值 + 动作链 + 输出**，用户自建、不安装 | 新增数据模型 + CRUD + UI |
| D3 | 插件参数：宿主透传 `options`，前端无声明 | 插件自带 `params.schema.json`，宿主 schema 驱动渲染 | 新增 schema 机制 + ParamForm |
| D4 | 结果行：`status` 三态打头（判定视角） | `result(ok/failed/skipped)` 必填；`verdict` 仅判定类；manifest `kinds:[judge|process]` | DB/API/UI 都要拆 |
| D5 | 处理动作（resize/convert/compress/watermark/upscale/inpaint…）散在网关 `/op/*` | 收成插件：**图片处理插件**（几何/格式/压缩/水印）+ 抠图插件 + 超分插件，`kinds=process` | 新增 3 张插件实现卡 |
| D6 | 插件页 = 安装/回滚/运行态（PackManager 改造） | 插件详情页三段：能力说明 / **我的规格** / 参数默认值（只读）+「新建规格」「先跑 1 张」 | PackManager 二次改造 |
| D7 | 内置功能（打标/查重/EXIF/OCR/向量）与插件边界未写死 | 内置功能**不重包**，加守卫测试 | 新增 1 张小卡 |
| D8 | 门禁 K07 = 插件 vs Legacy 引擎逐行对比 | Legacy 待删 → 对比改**冻结基线 JSON**；并新增 process 插件的产物断言门禁 | 门禁卡拆两张 |

---

## 2. 既有卡处置表（逐卡给动作）

### 2.1 保持不动（与新设计一致）

| 卡 | 标题 | 理由 |
|---|---|---|
| t_7aba77f3 | IP-K00 旧引擎 10 张基线快照 | 冻结基线来源，D8 靠它 |
| t_53a6242b | K07-FIX-I R0 语义收敛（done） | 规则语义在插件内，方向正确 |
| t_9224da8d | K07-FIX-J R3 多实例归属（blocked） | 恢复后继续，方向正确 |
| t_3e9c3028 | IP-K06 测试体系 | 不涉及对象模型 |
| t_6e8888ca | IP-K13 lama 模型内置 | 去水印插件的模型底座 |
| t_81f9cecd | K11 跟进 SDK 样例包同步 | 插件侧，正确 |
| t_6bb98335 | ECOM-PACK-1 语料 20 张 | 作为语料 v1 的真实底图池 |
| t_6cb81176 | K07-FIX-D2 性能二轮 | 已作废保留（用户改判），不动 |
| t_b832b97a / t_72daf400 / t_ed130167 | B4 / B5 / 计费对账 | 与插件设计无关，不动 |

### 2.2 改卡体（方向保留，口径要按新设计重写）

| 卡 | 改什么 |
|---|---|
| t_cb75c8ed IP-K01 协议定稿 | 补一节：`options` 由插件 `params.schema.json` 声明；manifest 增 `kinds`；这层**不改 api_version**（向后兼容：无 schema 的插件按自由 options 处理） |
| t_5118ce43 IP-K02 pack-host | manifest 解析增 `kinds` + `resources/params.schema.json` 加载；缺省时回退 |
| t_06c17b35 IP-K05 执行路径切换 | 注明「双跑开关」随 Legacy 删除一并移除，不得长期挂开关 |
| t_fea33411 IP-K08 .dampack 安装 | 安装期校验：manifest `kinds` 合法 + `params.schema.json` 可解析；非法拒绝安装（不是运行期才报） |
| t_77ba4297 IP-K10 UI 集成验收 | 验收步骤改成：装插件 → 详情页看到参数（未装灰态/已装可编辑）→ 新建规格 → 检查/处理跑一次 |
| t_f537e936 IP-K11 Python SDK | SDK 增：`params_schema` 导出约定 + 结果行构造器（`result/verdict` 二选一，处理类禁填 verdict） |
| t_2653e132 IP-K12 房产包 | 改名「房产图处理插件」，并要求 manifest `kinds:["judge"]`；仍作为"宿主零改动"证明 |
| t_a0fb0fd8 IP-K15 真机 E2E | 真机步骤按新入口（检查/处理 + 选规格），不是"选行业包+切引擎" |
| t_d4228e63 IP-K16 收口（blocked） | **砍到三件**：①rules/taxonomy/report 元数据只住插件 resources ②删 `packs/<行业>/*.yaml` + `core-config` 四个 `include_str!` + `ComplianceEngine::Legacy` + pipeline steps ③命名统一「<领域>处理插件」+ UI 删「行业包（合规判定规则）」区块。原第 4 件「装插件即含规则、一集绑一插件」依赖规格实体 → **移出到 N1/N4** |

### 2.3 拆卡（一张过重，按新设计切开）

| 原卡 | 拆成 |
|---|---|
| t_dc40d71f IP-K07 等价门禁（todo） | **N13** judge 门禁（冻结基线 JSON 逐行比对，10 张语料，硬不一致=0）＋ **N14** process 门禁（处理产物断言：尺寸/格式/体积/水印位置/白底偏差/超分尺寸，7 需求×10 张） |

### 2.4 历史卡不改（产物由新卡清理）

| 卡 | 说明 |
|---|---|
| t_327ce2e2 / t_c3dd20af | 生成/校正 `packs/ecommerce-amazon` 四件 YAML —— 产物属于被废弃对象，由 t_d4228e63 删除；卡保持 done 不改，作为历史可追溯 |
| t_cbef633a / t_d190732b / t_002dd104 等真机批次 | 历史验收记录，不改 |

---

## 3. 新建卡清单（14 张，含顺序与依赖）

### Wave 0：无代码依赖，可立即做

**N10 语料 v1：13 需求 × 10 张 + 逐图 ground truth + 断言 runner**
- 脚本已写：`damdesk-testing/scripts/build_plugin_corpus.py`（真实底图 pool：`/data5/damdesk-ecommerce-pack/corpus/pass_fixtures` 10 张白底商品图、`fail_fixtures` 10 张场景/低质图；派生 = 真实底图受控变换，参数即真值）
- 判定类 6 需求：`white_bg`（A3）、`size`（A1/A2）、`subject_ratio`（A4）、`overlay`（A5，含"包装印刷文字不算叠加"反例）、`domain_gate`（A0）、`multi_instance`（R3 归属）
- 处理类 7 需求：`resize`、`convert`、`compress`、`watermark`、`white_bg_fill`、`smart_crop`、`upscale`
- 产出：`/data/damdesk-test-corpus/v1/{audit,process}/<need>/*.jpg|png` + `labels/<need>.json`（逐图 expect）+ `manifest.json`（来源统计 native/derived）
- 验收：13 × 10 = 130 张齐全、labels 可被 runner 直接断言、`provenance` 统计与 manifest 对得上、抽 5 张人工核对内容与标签一致
- 已知 bug 已修：`f0010.jpg` 命名（`f00{n}`→`f{n:03d}`）；上次跑到 49/130 中断，需重跑全量

### Wave 1：内核契约（三张可并行，N1 依赖前两张）

**N1 规格实体（规格 = 交付目标配置）**
- 数据：`plugin_spec(id, plugin_id, plugin_version_pin, name, industry, rules[], thresholds{}, actions[], output{}, params_diff{}, created_at, updated_at)`
- 语义：规格不安装、不分发；同一插件下多规格（淘宝主图 / 亚马逊主图 / 我的 800px webp）
- API：`GET/POST/PATCH/DELETE /api/specs?plugin=<id>`；`POST /api/specs/{id}/try`（先跑 1 张）
- 迁移：既有"行业包"里可映射的规则选择 → 生成默认规格（ecommerce-amazon / ecommerce-taobao / realestate 各一条），阈值取原 YAML 值，禁止静默丢阈值
- 验收：CRUD+单测；一条规格可被 job 引用；删除插件后规格标记"插件已卸载"而非静默消失

**N2 参数 schema 机制**
- manifest/资源：`resources/params.schema.json`（JSON Schema 子集：type/title/default/min/max/step/options/when/group/advanced/required/schemaVersion）
- 宿主：加载+校验+`GET /api/plugins/{id}/schema`；非法 schema 拒绝安装
- 合并语义：`effective = schema.default ⊕ 规格 diff ⊕ 任务覆盖`；`when` 不成立的键剔除；**下发全量值**给插件 `options`
- 验收：单测覆盖 6 类控件/条件显隐/默认值合并/非法 schema 拒装；`options` 断言为全量而非 diff

**N5 结果契约改造（result / verdict 拆分 + kinds + result_columns）**
- 结果行：`result(ok|failed|skipped)` 必填（failed 带 error、skipped 带 skipReason）；`verdict` 仅 `kinds` 含 judge 时必填；`artifacts/evidence/fields` 保留
- DB 迁移：verdict 行改可空 + `kinds` 落插件安装记录；旧数据 backfill（原有判定行 → `result=ok` + verdict 原值）
- 渲染：`result_columns` 声明驱动；judge 插件追加审核列与复核动作
- 硬断言：处理类插件返回 verdict → 拒收并记错（防"处理冒充判定"）
- 验收：迁移脚本幂等 + 回读校验；两类插件各自的 API/UI 单测；反向验证（去掉拒收逻辑测试必须 FAIL）

### Wave 2：插件实现（依赖 Wave 1 的 N2+N5）

**N6 图片处理插件**（`kinds:["process"]`）—— 把 resize/crop/convert/compress/watermark/rename 收进一个插件多选项
- 复用现有网关 `/op/*` 实现，包成 manifest + params schema；产物 = 资产单工作版本
- 验收：与 N10 的 `resize/convert/compress/watermark/smart_crop` 需求逐项 PASS；禁止产生 verdict

**N7 抠图/白底插件**（`kinds:["process"]`，u2net 本地模型）
- 输出 PNG 透明或纯白填充；边缘不得鬼影
- 验收：N10 `white_bg_fill` 10 张：输出边框带偏差 ≤3、主体完整、幂等（已成白底再跑不变）

**N8 超分插件**（`kinds:["process"]`，Real-ESRGAN）
- 2x/4x；CPU 档限并发，manifest 声明耗时档
- 验收：N10 `upscale` 10 张：输出尺寸 = 期望、PSNR 对参考图记录且不低于阈值

**N9 合规检测插件规格化**
- 插件内规则目录不变（规则实现+默认阈值在插件）；**平台差异 = 规格**：`ecommerce-amazon`（A0/A1 500/A2 1600/A3/A4 0.55~0.85/A5/A6）、`ecommerce-taobao`、`realestate`
- 验收：同一插件切两条规格 → 判定结论按各自阈值变化（可复现）；同一图同规格跑 3 次翻转=0

### Wave 3：UI（N3 依赖 N2，N4 依赖 N1+N3）

**N3 通用 ParamForm**：控件映射表实现（number→滑杆/输入、enum→分段/下拉、boolean→开关、color、asset_ref、range）+ 分组/高级折叠 +「先跑 1 张」
**N4 插件详情页三段**：能力说明（只读）/ 我的规格（增删改）/ 参数默认值（只读）；未安装=灰态+"装好后可用"；**不放**任务与结果
**N12 入口收敛**：导航只留「检查」「处理」；入口选**规格**（单规格默认选中）；结果在任务中心按 job 查看
**N11 内置功能不重包守卫**：架构测试断言 打标/查重/EXIF/OCR/向量 不出现在 pack-host 依赖图与插件 manifest 中（防回归）

### Wave 4：门禁与交付

**N13 judge 门禁**（替代 t_dc40d71f）：10 张语料 × 冻结基线 JSON，`ruleId+status+score` 逐值一致（VL 只比 status 且翻转=0）；硬不一致=0 才 done
**N14 process 门禁**：N10 的 7 个处理需求 × 10 张全部断言 PASS，且产物全部落"资产工作版本"、manifest 可复现（同参数重跑 sha256 一致）
**N15 出包 + 真机批次**（依赖 Wave2/3 全部合并）：一次装机跑「检查×2 规格 + 处理×3 动作链」；真机单实例，禁并行装机

依赖图：
```
N10 ─┐
     ├─(语料供给)─→ N6/N7/N8/N9 验收 ──→ N14
N2 ─┬→ N1 ─→ N9
    └→ N3 ─→ N4 ─→ N12 ──────────────→ N15
N5 ─┴→ N6/N7/N8
N11（独立）                    N13（独立，依赖 N9）
```

---

## 3.5 卡粒度硬规则（2026-10-22 用户口径：插件别做大，避免超时，必要时拆子任务）

单卡上限（超了就拆，不许"一张卡跑到底"）：
1. **一个卡 ≤ 1~2 个 need**（语料的一个 need 就是一个验收单元）；插件可以是一个，实现必须分组拆卡，最后用一张"装配卡"合流。
2. **一个卡 ≤ 1 个 API 面**（契约 / 迁移 / 渲染 / UI 组件，不混）。
3. **预估 worker 时长 ≤ 45 分钟**；预计更长的必须拆（真机批次、全量回归这类本身是"批处理"的除外，它们按"一次装机跑完"计一张）。
4. 拆出来的子卡必须**各自可独立验收**（有自己的断言 + 报告），禁止"子卡 done 但只能和别的卡拼起来才验证"。
5. 所有卡体写清"不在本卡"的清单，防止 worker 顺手扩范围导致超时。

按此规则调整后的插件卡：

| 卡 | 范围 | parents |
|---|---|---|
| N6a `t_17e0a672` | imaging·几何：resize（不放大）+ crop（比例/锚点） | N2, N5a |
| N6b `t_83afd618` | imaging·编码：convert（含 alpha）+ compress（目标体积） | N2, N5a |
| N6c `t_6b2d45e4` | imaging·叠加：watermark（五锚点）+ rename（模板/冲突） | N2, N5a |
| N6d `t_979f61d2` | imaging·装配：manifest + 汇总 params.schema（**一个插件多动作**，不写算法） | N6a, N6b, N6c |
| N5a `t_7fa70669` | 契约：result/verdict 拆分 + kinds 解析 + 处理类禁 verdict（不落库不渲染） | — |
| N5b `t_4c0b3767` | 落地：DB 迁移 + result_columns 渲染 | N5a |

N7（抠图白底，1 need）、N8（超分，1 need）、N9（合规插件规格化，6 need 但同属一个契约面）保持单卡；N9 若实测超时再按规格（amazon / taobao / realestate）拆三张。

## 4. 卡体统一模板（新建卡都带这三节）

1. **口径锚点**：链接 `plugin-design-and-ui.md` 对应章节，禁止在卡里重述设计（避免二次解释走偏）
2. **硬边界**：不碰 pack-host 协议（除 N2 的向后兼容字段）、不碰真机（除 N15）、不改内置功能实现（N11 守卫）
3. **验收三件套**：单测/L0 原始输出 + 行为证据（命令与关键日志行）+ 报告落 `reports/<task_id>-*.md`

---

## 5. 恢复执行清单（等用户放行后按序做）

1. `systemctl --user start hermes-kanban-daemon-soujpg.service`
2. unblock 并继续：**t_9224da8d**（K07-FIX-J，改动量小且与 N9 相关）
3. **改卡体**（§2.2 九张 + t_d4228e63 砍到三件）；修改前先把旧 body 备份到 `reports/card-bodies-backup/<task_id>.md`
4. 建卡顺序：**N10 → N2 → N5 → N1 →（并行）N6/N7/N8 → N9 → N3 → N4 → N12 → N11 → N13 → N14 → N15**
5. t_dc40d71f 标 `archived`（由 N13/N14 取代），并在归档说明里写"新设计拆分为 N13/N14"
6. 全部合并后跑一次全量 L0 + L1，再按 N15 上真机


---

## 7. 语料驱动 E2E（2026-10-22 用户口径：统一 E2E 一律基于语料 v1）

**冻结（2026-10-22 用户口径：测试集图片保持固定）**：语料 v1 构建完即冻结 —— 生成 `CHECKSUMS.sha256`（逐文件 + 语料级 digest）、写 `FROZEN` 标记、全部文件设只读。**图片不得原地改（改图必须新开 `v2` 目录、禁止增删文件）**；若测出 label 本身写错，允许 labels-only 修订并升小版本 `v1.x`（像素 sha256 必须逐张不变，修订原因登记 `manifest.revisions`、重算 CHECKSUMS、重跑 `--verify`）。测试与门禁只读引用 v1 固定路径。校验命令：`python3 build_plugin_corpus.py --verify --out /data/damdesk-test-corpus/v1`（不一致退出码 2）。N13/N14/N15 三张卡开工前必须先跑一次 --verify 并通过。

> **语料 v1.1（2026-10-23, t_4e52ec0d）**：仅 labels/manifest 修订，140 张图片 sha256 与 v1.0 逐字节一致。
> ① `upscale.out_size` 更正为「输入 × scale」——原稿误写缩图前尺寸，奇数边原图（943/921/999）在 2x 下只能得到 942/920/998，up_03/06/08 三张期望不可达；新增 `orig_size`/`gt_reference_size`/`psnr_crop:"min"`/`odd_edge_note`。
> ② `smart_crop.subject_bbox` 改为从磁盘像素按 robust 规则复算（严格阈值会把 JPEG 近白振铃算进主体，sc_05 偏 21px、sc_10 偏 13px）；新增 `subject_bbox_raw`/`subject_bbox_rule`/`subject_bbox_tol_px:2`。
> **下游注意**：`crates/pack-host/tests/upscale_pack_e2e.rs`（N8/t_daca594a 分支）里 `assert_eq!(exact, 7)` 是旧 label 的产物，v1.1 下应改成 10。

**单一来源**：`/data/damdesk-test-corpus/v1/`（N10 产物）。任何插件相关测试——单测、L1 集成、门禁、真机批次——**不得另造图片、不得临场抓图**，只能引用语料 v1 的 `need` 分组与 `labels/<need>.json` 里的 expect。

**断言只有一套**：`labels/*.json` 即期望值；runner 只做"读 labels → 调 API → 断言"，禁止在 runner 里硬编码新期望（期望漂移必须回改 labels 并说明原因）。

| 阶段 | 用哪些 need | 断言 |
|---|---|---|
| N6 图片处理插件（L1/单测） | `resize` `convert` `compress` `watermark` `smart_crop` | 输出尺寸/格式/体积/水印位置/主体 bbox 完整，全部对照 labels.expect |
| N7 抠图白底插件 | `white_bg_fill` | 输出 `out_bg_dev_max<=3`、主体完整、幂等（再跑不变） |
| N8 超分插件 | `upscale` | 输出尺寸 = 期望；PSNR 对 `_ref_*.png` 达标 |
| N9 合规插件 × 规格 | `white_bg` `size` `subject_ratio` `overlay` `domain_gate` `multi_instance`（6×10） | 每条规格下逐图 verdict 与 labels.expect.verdict 一致；同图同规格 3 次翻转=0 |
| N13 judge 门禁 | 上述 6 个判定 need | 与冻结基线 JSON 逐行比对，硬不一致=0 |
| N14 process 门禁 | 上述 7 个处理 need | 全断言 PASS + 产物落"资产工作版本" + 同参数重跑 sha256 一致 |
| N15 真机批次（统一 E2E） | **全部 13 个 need 一次性导入**（130 张，导入后先断言 `/api/library/assets` total 增量 = 130） | 检查：2 条规格（淘宝/亚马逊）各跑全量判定并抽查 10 张对照；处理：3 条动作链各跑一组；导出：manifest 逐行对账；全程截图 + raw json 落 `reports/evidence/<task>/` |

N15 的硬性要求（写进卡体）：
1. 导入语料 v1 目录（不是重新采集），导入后先核 total 增量；空库或导入失败记 INFRA，不算产品结论。
2. 一次装机跑完全部项（真机单实例），禁止逐项重装。
3. 逐项 PASS/FAIL 表里必须写 `need/file` 与 labels 里的 expect 值，FAIL 原样保留不美化。
4. 真机结果与容器内 N14 结果不一致时，以"两边都跑同一批 need + 同一 expect"定位差在环境还是代码，禁止各跑各的图。

## 8. 风险

| 风险 | 处置 |
|---|---|
| 删 Legacy 后门禁失去对照 | N13 用冻结基线 JSON（t_7aba77f3 产物），不执行 Legacy |
| 规格与插件默认值双写漂移 | 规格只存 diff；默认值唯一定义在插件 resources（N2 单测钉死） |
| 处理类插件出现 verdict | N5 拒收 + 反向验证测试 |
| 打标/查重被误包成插件 | N11 架构守卫测试 |
| 语料派生样本被当成"原生真实" | manifest 强制 `provenance` 统计 + 报告必须列 native/derived 比例 |
| 一次性改造过大 | 按 Wave 切卡，每 Wave 结束跑 L0，禁止跨 Wave 合卡 |
