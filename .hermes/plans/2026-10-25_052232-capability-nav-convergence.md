# 能力导航收敛：能力库 + 可固定（pin）+ 运行态归任务中心

> 讨论裁决（2026-10-25）：插件（能力）变多后，左侧 NAV 逐个显示的形态会失效。
> 收敛原则 = **发现归能力库、高频归 pin、运行态归任务中心、搜索归命令面板**，四个关注点各有唯一归属。
> 本 plan 只规划，不执行；阈值未到（当前已装能力 ≤6）不提前切换。

## 1. 背景与现状（代码事实）

- 主导航能力区由已装包注册表驱动（KI-35 P1，`web/src/lib/capabilityNav.ts`）：
  `useCapabilityNav()` → `CapabilityNavItem[]` → `App.tsx` 的 `CapabilityNavSection`
  渲染在「打标任务」与「设置」之间，每个已激活能力一项，装了出现、卸载消失。
- 能力名/一句话描述目前由 `CAPABILITY_META` 映射维护（后端 `plugins[]` 尚未下发
  manifest 用户面名称，文件头 TODO 已记）。
- 能力详情页统一在 `/processing/:pluginId`（`PluginDetail.tsx`，三段布局 KI-35 P2）。
- 命令面板已有（`components/CommandPalette.tsx`，E3），PAGES 项覆盖图库/搜索/打标任务。
- 任务状态在侧栏底部 + `/tasks` 处理记录页（硬边界 5：跑与看结果在任务中心）。
- 已装能力现状 5 个（ecommerce / realestate / segment / upscale / imaging），未超阈值。

## 2. 裁决口径（勿重议）

1. **「只显示正在运行的插件」否决**：运行是瞬态、发现是常态；插件不是常驻服务而是
   批处理 job，运行态唯一归属是任务中心，投影到导航 = 第二入口，违反 §8 第 5 条。
   没在运行就没有入口 → 无法启动，死循环。
2. **NAV 不按安装数量扩张**，按用户 pin 数量扩张（上限 4）。
3. 注册表驱动原则（装了出现、卸载消失）保留，只是出现位置从「NAV 一级」改为
   「能力库页 + 可选 pin」。`capabilityNav.ts` 的数据源与 kinds 语义不动。
4. 组织维度：能力库页按「检查 / 处理」两动词分组（对齐两动词三名词裁决），
   组内平铺卡片；≤6 时也可先不分组，切换时机见 §3 阈值。

## 3. 触发阈值（不到点不动）

| 已装能力数 | NAV 形态 |
|---|---|
| ≤ 6 | 维持现状：能力区逐项直接显示（可发现性最好） |
| > 6 | 切换：NAV 只留一个「处理」入口 + 用户 pin 项（≤4），其余进能力库页 |

实现按「切换开关」做，不做运行时分支逻辑：阈值达到时一次性切卡落地，
阈值判断是建卡的依据，不是代码里的 if。

## 4. 目标形态（切换后）

### 4.1 NAV 结构

```
浏览     图库 / 搜索
处理     打标任务（第一档内置，保留）+ 能力库（/processing，固定项）
         ── pinned（用户 pin 的能力，≤4，逐项直达 /processing/:id）──
系统     设置
状态区   侧栏底部（任务/回收站，不动）
```

- 「能力库」固定项取代现在的逐能力列表；pinned 区夹在能力库与设置之间，
  空时不渲染（同「重复 N 组」徽标的条件渲染口径）。
- pin 来源：能力库卡片 / 详情页的「固定到侧栏」，再点取消；pin 列表存 localStorage
  （`pinnedCapabilities`），不进后端、不进 URL（是用户偏好不是状态）。
- pin 超上限 4：第 5 个 pin 时提示先取消固定一个（不自动挤掉）。

### 4.2 能力库页 `/processing`（复用现有路由根）

- 数据源复用 `useCapabilityNav()`，零新接口。
- 每卡：能力名 + 一句话 blurb（`capabilityMetaOf`）+ 我的方案数（`/api/specs` 已有）
  + 两个动作「打开」「先试一张」（先试一张入口对齐详情页既有动作，不在库页新造
  执行路径，只是带着能力上下文跳详情页）。
- 卡片按 kinds 分「检查 / 处理」两组（judge / process，`CapabilityNavItem.kinds` 已有）；
  单组时不出组标题（平铺）。
- 空态沿用 `CapabilityNavSection` 现有引导文案（去设置安装），同一份组件抽出复用。
- 装了出现、卸载消失：库页与 NAV 共用同一 hook，天然一致。

### 4.3 运行态（零改动）

任务中心 / 侧栏底部任务状态徽标 / toast 已有且符合裁决，**本 plan 不碰**。
导航与能力库不显示任何运行中状态。

### 4.4 命令面板

`CommandPalette` PAGES 增加 `/processing`（能力库）；能力项注册为动作
「用〈能力名〉处理选中图片」→ 深链 `/processing/:id?ids=…`（复用 sendToProcess
的选中集语义）。顺带修 F1（`/compliance` 深链不在 PAGES）——切卡时一并核对。

## 5. 改动清单（预估）

| 文件 | 改动 |
|---|---|
| `web/src/App.tsx` | `CapabilityNavSection` 改为「能力库固定项 + pinned 区」；`NAV`/`NAV_ITEMS` 相应调整 |
| `web/src/pages/Process.tsx` | 现动词运行页迁至 `/processing/run`（或保留深链重定向）；`/processing` 根渲染能力库 |
| `web/src/lib/capabilityNav.ts` | +`usePinnedCapabilities()`（localStorage 读写 + 上限校验）；导出供库页复用 |
| 新 `web/src/pages/CapabilityLibrary.tsx` | 卡片网格 + 两动词分组 + 空态引导 |
| `web/src/components/CommandPalette.tsx` | PAGES 补能力库；能力动作注册 |
| `web/tests/*.dom.test.tsx` | NAV 断言（tagBrowser NAV=5 那类）同步更新；新增库页/pinned 用例 |
| `scripts/ui_copy_guard.py` | 无新增禁词，跑通即证据 |

注意路由兼容：现有 `/processing/:pluginId` 深链与 `/plugins/:pluginId` 重定向都不动；
能力库占 `/processing` 精确路径，不动子路由。

## 6. 验收口径

1. 静态：`grep` 主导航代码零命中具体插件名（总纲 §8 第 10 条不变）；
   `ui_copy_guard.py` 禁词 0 命中。
2. 单测（jsdom，pageHarness 托架）：NAV 固定项 / pinned 0·1·N·超上限 / 库页
   两动词分组 / 空态引导 / 深链兼容各一例。
3. 真机 headless 走查（沿用 `ki35_p4_ui_walkthrough.sh` 模式补一段）：装包→库页出现、
   卸载→消失；pin→NAV 出现、取消→消失；命令面板动作深链可达。
4. 证据落 `reports/`，known-issues 回写。

## 7. 明确不做

- 不按「正在运行」过滤导航（已否决）。
- 不做能力库的搜索/筛选控件（≤20 个卡片网格不需要，命令面板已覆盖搜索式发现）。
- pin 不做拖拽排序（localStorage 里按 pin 时间倒序即可，有真实需求再加）。
- 阈值未到不提前切换、不写运行时数量分支。
- 运行态展示零改动。

## 8. 开放问题（切卡前裁决）

1. `/processing/run`（动词运行页）与能力库的路由切分：库页占根、运行页挪子路由，
   还是库页做 `/capabilities` 新路由、`/processing` 保持运行页语义？（倾向后者，
   改动面更小、深链零破坏——切卡时定）
2. 库页是否直接暴露「新建方案」快捷（现在只在详情页）？倾向不做，一个能力一个入口。
