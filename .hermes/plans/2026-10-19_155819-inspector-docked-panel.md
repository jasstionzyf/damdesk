# 检查器改右侧非模态抽屉（Inspector Dock）

日期：2026-10-19
用户裁决：检查器作为弹出框（与 packs Drawer / export Dialog 既有模式一致）；自动弹出，手动关闭后本会话不再自动弹；宽度 360（<640 全宽）；收掉 Lightbox 的 I 信息面板。

## 目标

1. 检查器不再常驻占宽：改为右侧**非模态**抽屉（docked panel），选中资产自动滑出，不遮罩、不阻断点图。
2. 主区恢复宽度：1280 视口主区从 512 → 832，1024 从 440 → 576，消除「中文竖排」。
3. 一个能力一个入口：详情只在检查器抽屉里看，Lightbox 的 I 信息面板删除。

## 现状（已核实）

- 布局常量：`web/src/pages/Gallery.tsx:150-152`（INSPECTOR_W=320 / COMPACT=288 / STRIP=48），
  计算在 1560-1572；`treeFits = vw >= NAV_W + TREE_W + inspectorW + minGridW` —— 检查器常驻会挤掉侧栏树。
- 检查器外壳：`web/src/components/Inspector.tsx:136`（未选中 48px 竖条）、`:155`（单选）、`:197`（多选）、`:367`（另一分支），
  全是 `w-72 shrink-0 border-l`（≥1280 `xl:w-[320px]`）。
- 挂载点两处：`Gallery.tsx:1642`、`SmartFolderPage.tsx:284`。
- Overlay 体系已存在：`web/src/lib/modal.tsx` —— `useOverlay('dialog'|'drawer', name, extraKeys)` 走
  `?dialog=`/`?drawer=`；`OverlayShell` 是**模态**（`bg-black/40`、`aria-modal`、遮罩点击关闭、焦点陷阱）。
  现有用例：packs（drawer）、export / dedup-scan（dialog）。
- Lightbox 的 I 键：`web/src/components/Lightbox.tsx:144`（`k === 'i' || k === 'I'` → `setShowInfo`），
  渲染在 `:370`（`data-testid="lightbox-info"`）。测试中无引用。

## 方案（最简）

### 1) `web/src/lib/modal.tsx`：新增非模态抽屉 `DockedPanel`

不复用 `OverlayShell`（它是模态，改它会牵动 packs/export/dedup 的行为）。新增独立组件：

```tsx
export function DockedPanel({ open, onClose, title, subtitle, testId, widthClass, footer, children })
```
- `open=false` → 返回 null。
- 外层：`fixed inset-y-0 right-0 z-40 flex` + **无遮罩层**（不渲染 `bg-black/40`、不占满屏）。
- 内容：`role="complementary"`（不用 `aria-modal`），`w-[min(360px,100vw)]`，`<640 → w-full`。
- Esc：窗口级 keydown 关闭（capture=false），`stopPropagation` 让 Gallery 的树抽屉 Esc 不重复触发。
- 不 trap focus；点击面板外**不**关闭（否则无法边看图边看详情）。
- header 复用标题 + 关闭按钮（`data-testid="${testId}-close"`）。

### 2) `web/src/components/Inspector.tsx`：加 `variant` prop

- 新增 `variant?: 'panel' | 'docked'`（默认 `panel`，零行为变更）。
- `docked` 时三处外壳改成 `flex h-full w-full flex-col`（去掉 `w-72 shrink-0 border-l` 与固定宽）。
- 未选中态（`:136` 的 48px 竖条）在 docked 下不渲染 —— 抽屉只在有选中时存在。
- 其余内容/文案/testId 全部不动。

### 3) 抽一个 `InspectorDock`（避免两处重复接线）

新文件 `web/src/components/InspectorDock.tsx`，供 Gallery 与 SmartFolderPage 共用：

```tsx
<InspectorDock
  open={inspOpen} onClose={closeInsp}
  hasSelection={!!activeAsset || (batchAssets?.length ?? 0) > 1}
  autoOpenKey="gallery"        // sessionStorage 记忆键前缀
  {...InspectorProps}
/>
```
内部逻辑：
- `useOverlay('drawer', 'inspector', ['ids'])` 提供 URL 真源（`?drawer=inspector`）。
- **自动弹出**：`hasSelection && !sessionStorage['dam.inspector.autoClose:<autoOpenKey>']` → 打开。
- **关闭记记忆**：用户点关闭或 Esc → 写 `dam.inspector.autoClose:<key>=1`（仅本会话），本次不再自动弹。
  重新手动打开（选中后点工具条「详情」按钮）时清掉该标记。
- 打开入口：网格工具条加「详情」按钮（`data-testid="inspector-toggle"`）；选中后自动开时不需要额外点击。

### 4) `web/src/pages/Gallery.tsx`

- 删除 `INSPECTOR_W*` 三个常量与 1560-1572 里 inspector 相关计算：
  `inspectorW` 恒 0 → `treeFits = vw >= NAV_W + TREE_W + minGridW`（侧栏树不再为检查器让位）。
- `Gallery.tsx:1642` 的 `<Inspector>` 换成 `<InspectorDock ...>`。
- Esc 优先级（显式定义，自上而下）：Lightbox → 对话框/其他抽屉 → 检查器 → 树抽屉 → 清空筛选。

### 5) `web/src/pages/SmartFolderPage.tsx`

`:284` 的 `<Inspector>` 换成 `<InspectorDock autoOpenKey="smart-folder">`，其余 props 不变。

### 6) `web/src/components/Lightbox.tsx`：收掉 I 信息面板

- 删除 `showInfo` state、`:144` 的 I 键分支、`:276` 的「信息」按钮、`:370` 的 `lightbox-info` 块。
- `ShortcutsHelp` 里如有「I 显示信息」条目同步删除（搜索 `lightbox-info` / `按 I`）。
- 保留 Lightbox 的 ←/→、Z、1-5 打分、X 拒选等既有键位不变。

### 7) 文档与口径同步

- `skills/damdesk/damdesk-repo/references/known-issues.md` 第 5 条：状态改为「方案已定，执行中/完成」，
  记 A 方案（检查器 drawer）为主方案，B（分档收栏）降级、C/D 保留。
- `skills/damdesk/damdesk-repo/references/ui-landing.md`：三栏 → 两栏 + 抽屉，列清路由与载体。
- `skills/damdesk/damdesk-testing/references/real-machine-coverage.md`：横向占用基线去掉检查器 288/320
  （改为导航 208 + 图库侧栏 240 + 主区），1280 下缩略图 ≥4 列的判据重算。

## 文件清单

改：
- `web/src/lib/modal.tsx`（新增 DockedPanel）
- `web/src/components/Inspector.tsx`（variant prop）
- `web/src/pages/Gallery.tsx`（常量、计算、挂载、工具条按钮）
- `web/src/pages/SmartFolderPage.tsx`（挂载）
- `web/src/components/Lightbox.tsx`（删 I 面板）
- `web/src/components/ShortcutsHelp.tsx`（删 I 条目，若有）

新增：
- `web/src/components/InspectorDock.tsx`
- `web/tests/inspectorDock.dom.test.tsx`

## 测试与验收

自动化：
1. `cd web && npx tsc -b` 通过；`node node_modules/vite/bin/vite.js build` 通过。
2. `npm test` 全绿；新增 `inspectorDock.dom.test.tsx` 覆盖：
   - 选中资产 → 抽屉自动打开（`data-testid="inspector-dock"`）；
   - 点关闭 → 再选另一张 → 不再自动开；
   - 手动点「详情」→ 重新打开，且自动标记被清除；
   - Esc 关闭且不触发树抽屉关闭；
   - URL `?drawer=inspector` 直接进入时打开（刷新/深链恢复）。
3. 既有测试修补：任何断言 Inspector 常驻宽度/`w-72`/`lightbox-info` 的用例（当前 grep 只有 Lightbox 自身，确认后改）。

人工（真浏览器，1024 / 1280 / 1440 三档视口）：
1. 主区实际宽度 1024≥576 / 1280≥832 / 1440≥992；扫无宽度 <160px 的文本容器（竖排消失）。
2. 选中图 → 右侧滑出 360 面板，不遮罩，能直接点下一张缩略图且内容跟随。
3. 关闭后本会话不再自动弹；重新打开一次后恢复正常。
4. 全屏 Lightbox 里按 I 无反应（面板已删），1-5 / X / ←→ 正常。
5. <640 视口面板全宽 + 关闭按钮可点。

## 风险与取舍

- **Esc 优先级**：检查器与树抽屉都用窗口级 Esc。用 capture 顺序显式定：Lightbox > dialog > docked panel > 树抽屉；
  docked 的 handler 需 `stopPropagation`，否则一次 Esc 关两层。
- **URL 键冲突**：`?drawer=` 是单值，packs 与 inspector 不能同屏共存。可接受（本来没有同时开的场景），
  plan 里不做多值改造。
- **无遮罩会不会误操作**：面板开着时点缩略图会切换内容（预期行为）；误触关闭靠 Esc / 关闭按钮，不做点击外部关闭。
- **SmartFolderPage**：该页没有多选批量，`batch` 传 `[selected]` 保持现状即可。
- **不做的事**：不做「锁定/常驻」开关（用户未要，A 方案已取代）；不做移动端手势抽屉；不改 `filter_where` 等后端。

## 执行顺序

1. `modal.tsx` 加 DockedPanel → 单测/typecheck
2. `Inspector.tsx` 加 variant（不改默认行为）
3. 新增 `InspectorDock.tsx` + Gallery 接线 + 布局常量清理
4. SmartFolderPage 接线
5. Lightbox 删 I 面板 + ShortcutsHelp
6. 补测试 + 跑 tsc/build/npm test
7. 真浏览器三档视口验收（CDP 量宽）
8. 回写 skills（known-issues / ui-landing / real-machine-coverage）
