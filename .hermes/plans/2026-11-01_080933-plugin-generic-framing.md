# 插件体系去合规中心化：主轴重写 + 契约改名（outputs 字段 + 单方法 run）

- 日期：2026-11-01（v3：judge/repair 双方法废止 → 单方法 run）
- 裁决来源：用户口头确认（本对话四轮：①命名主轴倒置 ②范式轴分析 ③kinds→outputs ④单方法化——
  repair 本质=处理，不是与执行并列的动词）。
- 性质：文档 + 契约改名（api_version v1.2 → v2.0）：manifest 字段 kinds→outputs、RPC 执行动词 judge/repair→run。
  宿主模块名/路径不动（层2 依旧顺势做）。

## 0. 裁决摘要

1. **主轴倒置**（修订 2026-10-22 裁决的命名口径；该裁决其余条款仍有效）：
   插件 = 通用批处理能力单元：**输入 = 用户客户端选择的一批图片，输出 = 每图一行结果（处理产物和/或判定结论）**。
   范式 = 输入输出形状：插件只负责对应的处理，宿主负责形状声明与编排。「行业」= 插件属性（domain），
   「合规」= 能力的一种——都不是类型名。
2. **对象命名（拍板项，可被用户推翻）**：类型名 = **插件（plugin）**，安装分发形态 = **插件包（.dampack）**；
   「行业包」降为俗语 = 绑定了行业 domain 的插件；「判定类插件」= outputs 含 verdict 的插件。
   用户面命名表（能力/方案/处理记录，2026-10-24 裁决）不变，**UI 零改动**。
3. **单方法化（2026-11-01 第四轮裁决，取代原「层3 冻结」——该冻结作废）**：
   v2.0 起 JSON-RPC 只有一个执行动词 **`run`**；judge/repair 双方法废止。理由：repair 不是与执行并列的动词，
   只是「产出 artifact 的一次执行」。判定/修复/处理都是 run 一次调用内部的事：
   - 判定类 run → verdict 行；处理类 run → artifact；判定修复类 run 一次出 verdict+artifact（行带 fixed）；
   - fix_policy 在 options 里门控只判不修（本来就在 options，方法选择这层是冗余的）；
   - 先判后修（人工复核后再修）= **两次 run**（参数不同），各自独立 job 留痕——贴铁律 4
     （一次执行 = 一个 job，manifest 记动作+参数可复现）；
   - 附带收益：执行语义（timeout/cancel/progress）只定义一遍；report 计数字段中性化（judged→rows）；
     单方法为 vNext 集合级调用留口（run 输入=集合句柄、输出=行流，将来加 input_scope 轴即可，不再动方法）。
   health / pack.info / report / cancel 保留。
4. **层2 顺势做（不变）**：宿主命名（compliance_api.rs、/api/compliance/*）本 plan 不动；
   下次动该模块的卡顺手改，不单开卡。
5. **契约字段改名（第三轮裁决）**：`kinds` → **`outputs`**，值 `judge` → **`verdict`**、`process` → **`artifact`**
   （组合：["verdict"] / ["artifact"] / ["verdict","artifact"]）。字段名直接回答那根轴（输出什么），
   值从场景动词变输出名词；与 §0.3 单方法化合并后，manifest 与 RPC 两层场景词汇全部清零。
   - **窗口理由**：开源仓刚发布、第三方插件作者为零，现在改是史上最便宜窗口；协议文档本就要重写，
     不改名会把旧词汇再正典化一遍，将来还得二次重写文档。
   - **协议版本**：api_version v1.2 → v2.0，manifest 规范、RPC 方法、conformance 同批。
   - **兼容口径（一次迁移，不留长期双轨）**：5 个官方包同批升 v2.0（manifest + run 实现）；
     新装只认 v2.0（出现 `kinds` 或 api_version<2 → 拒装 + 报错指路映射：
     "kinds 已更名 outputs，值映射 judge→verdict / process→artifact；执行方法 judge/repair 已合并为 run"）；
     已装 <2.0 包标记待升级，真机重新导入即恢复，**不做协议层 shim**。
6. **vNext 范式扩展（声明，不实现，升 api_version 时逐项设计）**：
   **轴纯度原则：outputs 只编码「输出是结论还是产物」这一根轴；范式演进一律加兄弟声明字段**
   （input_scope = per_asset|per_set、artifact_identity = working_version|new_asset_lineage、
   row_cardinality = 1:1|1→M|N→1，均独立闭集、各带缺省、走装机校验），
   **禁止往 outputs 塞场景值（select/synthesize 之类）——那是合规中心化的换马甲重犯。**
   ① 集合级调用：整集进 → 行流出，逐图是集合退化为单图的特例（realestate F3/F4 包内按目录自聚合 = 需求实锤）；
   ② 血缘产物：合成图 = 新资产 + parentAssetId 指向输入集，产物挂靠从「单工作版本」扩为「工作版本 | 新资产（带血缘）」，
   「结果集是视图不长第二个图库」边界不破；
   ③ 结果行 1→M：行外壳 assetId 单值变可选数组，或产物层独立于逐资产行。
   未来 manifest 示例（拼图插件）：`{"outputs":["artifact"],"input_scope":"per_set",`
   `"artifact_identity":"new_asset_lineage","row_cardinality":"n_to_1"}`——outputs 不加值，新范式 = 新兄弟字段。

## 1. 问题现状（2026-11-01 实查证据）

- 类型名「行业合规插件包」（damdesk-product SKILL.md「行业包只有一个对象」节）与其自身语义定义矛盾
  （同文件：「插件语义 = 通用批量处理」「行业是属性不是实体」）；抠图/超分/水印/成像包既不行业也不合规。
- 协议层：宿主对 process 包也只发 judge 方法（imaging 包靠 options.action 选动作）；repair 被处理类包用 -32601 桩掉
  （双执行动词本身就是场景残留：repair 只服务判定修复场景，却要求所有插件实现占位）；
  `kinds` 字段名不自解释 + 缺省 = ["judge"]（特例当默认；实测 5 包中 ecommerce 未显式声明）；
  结果行身份标识 ruleId、report 回 judged 计数——judge 是万能动词。
- 协议文档内部不统一：docs/industry-pack-protocol.md 首句自称「处理插件（industry processing plugin）」，方法却只有 judge/repair。
- 宿主面：扩展机制住在合规模块（crates/app-web/src/compliance_api.rs、/api/compliance/packs、入口 /judge）。
- 用户界面层已通用化（2026-10-24 禁词表 + 能力/检查/处理命名），**困惑集中在开发者文档 / skill / 协议层** = 本 plan 主战场。

## 2. 目标形态

导语母版（所有改动文件的唯一口径源）：

> 插件 = 一类图片批处理能力的可安装实现：输入一批图，输出每图一行结果（处理产物和/或判定结论）。
> 行业是插件的属性，合规是能力的一种。

outputs 语义表（写进总纲与协议文档；旧名 kinds 只出现在「历史字段」说明里）：

| outputs | 出结论 | 出产物 | 例 |
|---|---|---|---|
| ["verdict"] | ✔ | ✘ | 房产合规判定、智能挑选/筛选 |
| ["artifact"] | ✘ | ✔ | 成像六动作、抠图换背景、超分 |
| ["verdict","artifact"] | ✔ | ✔ | 电商判定+修复一条龙 |

协议动词表（v2.0）：`run`（唯一执行动词）+ health / pack.info / report / cancel。
判定/修复/处理 = run 内部行为，由 outputs 声明 + options（含 fix_policy）决定，不再有第二个执行方法。

## 3. 改动清单

### 层1-A skill 侧（~/.hermes/profiles/soujpg/skills/damdesk/）

1. `damdesk-product/SKILL.md`：「行业包只有一个对象：行业合规插件包」节重写为「插件只有一个对象：插件包（.dampack）」；
   保留 2026-10-22 裁决仍有效条款（展示元数据随包走 / Legacy 双轨收口四条 + 门禁先于拆 Legacy 的顺序硬约束），
   命名条款标注「2026-10-22 裁决，命名部分 2026-11-01 修订」；五支柱表 Industry Rule Packs 行措辞同步。
2. `damdesk-product/references/plugin-design-and-ui.md`（总纲）：§1 一句话换母版；§2 三档表「可安装插件」例子里
   合规检测与抠图/超分并列（去中心位）；§7 结果段强调 outputs 对称（artifact 类不得伪造结论的对偶 =
   verdict 类不得伪造产物语义）；新增 §10「遗留命名处置与 vNext」（§0.3~§0.6 落档：outputs 映射表、
   judge/repair→run 映射、compliance_api 顺势改、轴纯度原则 + 三扩展点，防后续会话重提或又理解回合规专用）。
3. `damdesk-product/references/plugin-and-pipeline.md`：「插件 = 一个行业/一类处理的完整能力（判定+修复+交付一次跑完）」
   改为通用定义（判定/修复/交付 = 种类组合，不是每个插件的固定形状）。
4. `damdesk-product/references/industry-packs.md`：标题与导语改口径（行业包 = 绑行业 domain 的插件；本文专讲这类包的
   判定语义铁律）；正文（domain gate / C10 / 验收语料）不动。
5. `damdesk-product/references/plugin-result-contract.md`、`plugin-param-schema.md`：导语各加一句母版 + 术语全量替换
   （kinds→outputs；judge/repair 语境适配 run）；执行时抽查正文是否残留合规框架措辞，有则同改。

### 层1-B 仓内文档（/data/projects/soujpg-dam-desktop）

6. `docs/industry-pack-protocol.md`：导语重写（处理插件主轴 + 母版）；api_version v2.0；manifest 规范 kinds→outputs
   （含旧名映射说明）；§3.3/§3.4 合并为 **run**（params = 输入集 + options；fix_policy 在 options；响应行按 outputs
   出 verdict / artifacts / fixed）；report 计数字段中性化（judged→rows）；cancel/timeout/progress 语义单方法只写一遍；
   「新插件必须显式声明 outputs（无缺省）」；文末新增「vNext 演进（声明未实现）」节：§0.6 全部 + 未来 manifest 示例。
7. `docs/damdesk-产品说明.md` §5：「一个行业 = 一个插件」→「一个能力 = 一个插件；行业是插件可带的属性」；
   安装步骤措辞同步。
8. `damdesk-repo/references/{plugin-dev-guide,industry-pack-plugin-arch,industry-pack-authoring,pack-host-runtime}.md`：
   标题/导语各加一句母版定位；正文全量术语替换（kinds→outputs / judge·repair→run / process→artifact，
   含示例 manifest 与 JSON-RPC 示例报文）。
9. `damdesk-flows` skill：走查 F 条目「插件」措辞（实查仅 1 处且已中性），预期不动；验收时 grep 复核。

### 层1-C 契约改名代码（与 item 6 同批，conformance 全绿为门禁）

10. 宿主（crates/app-web + pack-host 相关）：
    - manifest 解析/校验改 `outputs: ["verdict"|"artifact"]`（闭集、非空、无重复；出现 `kinds` 或 api_version<2 →
      拒装 + 映射指路报错；已装 <2.0 标记待升级）；
    - 调用点统一发 **run**（原 judge 派发与 repair 二段调用合并；先判后修 = 两次 run 各自 job）；
    - 下游消费点（verdict 必填校验、审核列追加、报告聚合）改读 outputs；report 响应字段 judged→rows。
11. `packs/plugins/{imaging,realestate,segment,upscale,ecommerce}/`：manifest 升 api_version 2.0 + kinds→outputs
    （ecommerce 补显式 ["verdict","artifact"]，消灭缺省依赖）；插件 server 模板 judge/repair handler → 单 run
    （realestate/ecommerce 修复逻辑并进 run 的 fix_policy 分支；imaging/segment/upscale 删 repair 桩）。
12. conformance 套件与 pack_lint：用例改 run + outputs（合并原 judge/repair 用例；补「api_version<2 拒装」「kinds
    拒装指路」两条）；pack_lint 强制显式声明 outputs（原 P2 项顺势并入）。

### 对外文案挂账（随既有批次走，不在本 plan 单做）

13. `damdesk-product/references/positioning-rollout-plan.md` 待改清单追加一条：对外描述插件 = 通用处理能力，
    合规判定是官方插件之一（README.md 实查 0 处「合规」，本次无需改）。

### 明确不做（防重提）

14. vNext 三扩展点实现（集合级调用/血缘产物/行 1→M）——真实需求出现再逐项设计开卡。
15. 宿主模块/路径更名（compliance_api → processing/plugins，层2）——只在下次动该模块的卡里顺势做。
16. 协议层旧版 shim（judge/repair→run 适配层）——不做：官方包同批升级，旧包重导入即恢复。

## 4. 实施顺序

总纲（item 2）先定母版与术语映射表 → skill 其余（1,3,4,5）→ 协议文档 v2.0（6）与代码同批（10,11,12：
同一卡内先改规范再改实现，conformance 全绿为门禁）→ 产品说明（7）→ repo 指南导语与替换（8）→
走查（9）→ 挂账（13）→ 验收 grep + conformance + 装机/拒装/双 run 路径验证。

## 5. 验收口径

- grep 验收（skill 目录 + 仓内 docs/ + packs/ + crates/）：
  - 「行业合规插件包」命中 = 0（唯一允许残留：SKILL.md 修订标注行，且必须出现在「已废止口径」上下文）；
  - 「一个行业 = 一个插件」命中 = 0；
  - plugin-design-and-ui.md §1 与 protocol 导语均含「输入一批图」；
  - protocol 含「vNext」节、api_version v2.0、单方法 run；
  - `packs/plugins/*/manifest.json` 与 crates 内 `kinds` 字段引用命中 = 0（协议文档/skill 的「历史字段说明」除外）；
  - crates/packs 内 judge/repair 方法派发与实现命中 = 0（同上例外）。
- 行为验收（容器 dam-dev）：
  - 5 个包重装全通过；imaging 跑一次六动作 run（处理路径）、realestate 跑一次 run（判定路径）、
    ecommerce 跑两段 run（先判后修，两次 job 各自留痕——铁律 4 回归）；
  - 旧 v1.2 包（构造旧 manifest）新装 → 拒装且报错含映射指路；已装旧包 → 标记待升级，重导入恢复；
  - conformance 套件全绿；pack_lint 对缺 outputs 的包报错。
- 零越界声明：git diff 限定 docs/、.hermes/plans/、packs/plugins/、pack-host/app-web 校验与调用代码、
  conformance/lint；不动 /api/compliance/* 路径、不动 UI、不加协议 shim。
- 历史裁决不篡改：2026-10-22 裁决保留语义，命名条款加修订标注。

## 6. 开放问题

- 无阻塞项。对象命名（插件 / 插件包）、字段命名（outputs / verdict / artifact）、单方法 run 均为拍板项，
  用户可推翻；推翻只影响母版与映射表用词，改动面不变。
