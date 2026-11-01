# 布局分区 + 低频功能 Dialog 化（计划 v2）

- 计划文件：`.hermes/plans/2026-10-17_174500-layout-zones-dialogs.md`
- 目标仓库：`http://172.25.45.244:13000/zhaoyufei/soujpg-dam-desktop.git`（`main`）
- 基线：`gitea/main` 最新（本计划撰写时 `5677971` + 待合并的 UI-DIALOG `wt/t_9642d1d1`）
- 面向读者：没有本仓库上下文的实现者

---

## 1. Goal

按产品功能把界面切成 **3 个常驻区域 + 1 组顶栏动作**，并把低频功能（设置、导出配置、行业包管理、自定义打标任务、回收站、失联核查、快捷键帮助…）全部收进**按钮 → 对话框**，让主视图只服务高频动作（看图 / 搜图 / 筛选 / 批量处理），固定横向占用从 736px 降到 ≤ 480px。

---

## 2. 功能分区表（产品功能 → 区域 → 载体）

| # | 产品功能 | 使用频率 | 归属区域 | 载体 |
|---|---|---|---|---|
| 1 | 图库浏览（网格/缩略图/Lightbox/检查器） | 极高 | **浏览区** | 常驻页面 `/` |
| 2 | 搜索（文件名/关键词、语义文搜、以图搜图） | 高 | **浏览区** | 图库工具条内联三模式（吸收原 `/search` 页） |
| 3 | 筛选（库视图/目录/标签/评分/合规判定） | 高 | **浏览区** | 图库左侧栏（可折叠，只留高频三段） |
| 4 | 批量动作（打标/评分/色标/移动/删除/导出） | 高 | **浏览区** | 选中后检查器 + 工具条 |
| 5 | 去重裁决 | 中 | **整理区** | 常驻页面 `/dedup` |
| 6 | 打标队列（模板/增量/全库重打/OCR） | 中高 | **处理区** | **处理页 tab①**「打标」（路由 `/tag-tasks`） |
| 7 | 合规判定与复核（三档/低置信度/反馈） | 中高 | **处理区** | **处理页 tab②**「合规」（路由 `/compliance`） |
| 8 | 智能文件夹 | 中 | 浏览区 | 侧栏折叠段（已有） |
| 9 | 导入 | 中 | 顶栏动作 | `＋ 导入` → 已有向导对话框 |
| 10 | **导出（基于选择集/当前筛选的一次性动作，无历史概念）** | 中低 | 顶栏动作 | `导出…` → **Dialog**（原 `/export` 页降级） |
| 11 | **设置（推理引擎/AI 打标/合规参数/存储清理/关于）** | 低 | 顶栏动作 | `⚙` → **Dialog**（原 `/settings` 页降级） |
| 12 | **行业包管理**（预览/激活/导入本地包） | 低 | 合规页 | 按钮 → **Dialog** |
| 13 | **自定义打标任务**（新建/编辑/删除） | 低 | 打标页 | 按钮 → **Dialog** |
| 14 | **回收站**（去重软删恢复/清空） | 低 | 整理区 | 按钮 → **Dialog** |
| 15 | **失联核查详情**（missing/unreadable 明细） | 低 | 浏览区 | 「失联 N」旁按钮 → **Dialog** |
| 16 | **快捷键帮助** | 低 | 全局 | `?` → **Dialog**（已有，`ShortcutsHelp.tsx`） |
| 17 | **任务中心**（长任务进度/取消） | 中 | 全局 | 侧栏底部状态条 → 抽屉（已有 `TaskCenterDrawer`） |
| 18 | 标签审核（`/tag-review`） | 低 | 处理区 | 打标页内按钮 → 现有路由保留，导航不暴露 |

**导航结果（2026-10-17 用户裁决后）**：`图库 / 去重 / 处理` **三项**（「浏览」分组：图库、去重；「处理」分组：处理）。顶栏右侧：`＋ 导入`、`导出…`、`⚙ 设置`、任务状态（侧栏底部）。
「处理」= 一个页面两个 tab：**打标**（`/tag-tasks`）+ **合规**（`/compliance`）。搜索、导出、设置不再占导航位。
**所有与"判定/合规"相关的浏览与筛选都只在处理页的「合规」tab 里，图库页彻底不出现合规内容**（侧栏段删除、工具条也不留"判定"下拉）。

---

## 3. 现状与证据（已实测，勿重复侦察）

| 事实 | 证据 |
|---|---|
| 固定横向占用 736px | `web/src/App.tsx` L118 `aside w-52`(208) + `components/LibraryTree.tsx` L266 `w-60`(240) + `components/Inspector.tsx` L90 `w-72`(288) |
| 检查器未选中仍占 288px | `Inspector.tsx` L88-95（`if (!asset)` 分支仍是 `w-72` + 「未选中图片」） |
| 顶栏无搜索输入 | `grep -c 'placeholder' web/src/App.tsx` → 0 |
| 导航 7 项平铺 | `App.tsx` L14-22 `NAV` 常量 |
| 路由 9 条全平铺 | `web/src/main.tsx` L23-53（含 `/search`、`/export`、`/settings`、`/tag-review`） |
| 响应式仅 5 处 | 全仓 `grep -oE '\b(sm|md|lg|xl|2xl):'` 只命中 Dedup(2)/Settings(1)/TagTasks(2) |
| Dialog 基座已存在（未合 main） | 分支 `wt/t_9642d1d1:web/src/lib/modal.tsx` 导出 `useModalDismiss`、`ModalCloseButton`、`modalStackDepth`（Esc 栈、焦点归还、点遮罩关闭都已实现并有 `tests/dialog-dismiss.dom.test.tsx`） |
| 设置页/导出页/合规页都是整页 | `web/src/pages/Settings.tsx`、`ExportPage.tsx`、`Compliance.tsx` |

---

## 4. 目标布局（ASCII）

```
┌────────────┬───────────────────────────────────────────────────────────────┐
│ DAM        │ [搜索框：文件名/语义/以图搜图 ▾]   ＋导入  导出…  ⚙   ☰ 任务(2) │
├────────────┼───────────────┬───────────────────────────────┬───────────────┤
│ 浏览        │ 库            │                               │ 检查器          │
│  ▣ 图库    │  全部 27      │         缩略图网格              │ (未选中时收起  │
│  ⧉ 去重    │  最近 24      │                               │  为 40px 竖条) │
│ 处理        │  未打标 0     │                               │               │
│  ⚙ 打标    │  失联 0 ⋯     │                               │               │
│  ⚑ 合规    │ 目录 ▾        │                               │               │
│            │ 标签 ▾ 🔍     │                               │               │
│ ─────────  │ 评分 ▾        │                               │               │
│ 进行中 2   │               │                               │               │
└────────────┴───────────────┴───────────────────────────────┴───────────────┘
   160px          折叠态 0 / 展开 240px          flex-1            0 / 288px
                 （合规判定筛选移到合规页，不再占图库侧栏）
```

固定占用：160 + 240(可折叠) + 288(可收起) = **展开时 688 → 常用态 400（侧栏折叠+检查器收起）**；≥1280px 屏幕缩略图区 ≥ 6 列。

---

## 5. 架构 / 方案

1. **Dialog 有 URL 状态**：低频功能用 `useSearchParams()` 的 `?dialog=settings|export|pack|custom-task|recycle|missing` 表示，关闭即删参数。好处：可深链、可被命令面板调用、刷新不丢、`/settings` 等旧路由用 `<Navigate replace>` 重定向到 `/?dialog=settings`（不破坏书签）。
2. **复用已有 Dialog 基座**（`web/src/lib/modal.tsx`：`useModalDismiss` + `ModalCloseButton`），不再新写关闭/ESC/焦点逻辑。
3. **不重写业务页**：`Settings.tsx`、`ExportPage.tsx` 的内容整体包进 Dialog 容器（只改外壳与内部 tab），请求与字段一律不动。


### 5.1 信息架构原则（2026-10-17 用户裁决，长期约束）

**区域 = 功能模块 / 层级；区域内含该模块的下级子功能；区域之间允许有关联。**

落成可执行的六条规则：

1. **层级最多两层**：区域 → 子模块（tab / 分段控件 / 弹窗）。禁止"区域 → 子页 → tab → 弹窗 → 二级弹窗"这种四层下钻；超过两层的功能说明它该拆成两步操作。
2. **每个区域有唯一默认落点**：进入区域先到主视图（如「处理」默认到打标 tab），子模块靠 tab 切换，不靠多级菜单展开。
3. **跨模块关联 = 带上下文的跳转，不复制功能**：功能只住一个区域（判定只在处理页），其他区域通过参数引用它（`?ids=…` / `?from=selection` / `?filter=…`），并提供返回原位。
4. **区域按用户任务划分，不按代码模块划分**：数据来自图库不等于入口在图库（例：判定筛选属"处理"，即使它筛的是图库数据）。
5. **可发现性兜底**：任何功能 ≤ 2 次点击到达，且必须在命令面板（⌘K）里可搜到 —— 这是层级化的纠错网，防止深层功能被埋。
6. **频率只影响载体，不影响层级**：高频留在主视图，中低频收 Dialog / Drawer，但**功能归属不变**。频率变了只换载体，不重排区域（避免每次产品迭代都动导航）。

**功能地图（Functional Map，新功能上线前必须先填这张表定归属）**

| 区域 | 子模块 | 载体 | 关联（带上下文跳转） |
|---|---|---|---|
| 浏览 | 图库（网格/Lightbox/检查器） | 页面 `/` | → 处理（`?ids=`）、→ 导出（选择集）、→ 去重（按图查重） |
| 浏览 | 搜索（文件名 / 语义 / 以图搜图） | 图库工具条 | → 处理（对结果打标）、→ 导出（结果集） |
| 浏览 | 筛选（目录/标签/评分/智能文件夹） | 图库侧栏 | → 处理（按筛选打标/判定） |
| 整理 | 去重（分组/裁决/回收站） | 页面 `/dedup` | → 图库（保留项）、回收站 Dialog |
| 处理 | 打标（模板/增量/OCR/自定义任务） | 处理页 tab① | → 合规（打完即判）、→ 图库（范围来源） |
| 处理 | 合规（三档/低置信度复核/包管理） | 处理页 tab② | → 图库（带判定筛选查看）、→ 打标（不合格重打） |
| 系统 | 设置（引擎/打标/合规/存储/关于） | 顶栏 ⚙ → Dialog/Drawer | 全部区域 |
| 系统 | 导出（一次性动作） | 顶栏 导出… → Dialog | 浏览区选择集/筛选 |
| 全局 | 任务中心、命令面板、快捷键帮助、导入 | 侧栏底部 / ⌘K / `?` / 顶栏 | 全部区域 |

**验收判据（本轮）**
- 任一新页面/功能若未在上表登记 → 视为未完成（PR 模板/评审检查项）。
- 每次交互路径 ≤ 2 次点击可达（深层用 ⌘K 兜底）。
- 跨区跳转必须携带来源参数并在目标区显示「来自：图库已选 3 张（清除）」。

---

## 6. 环境准备（实现者从零）

```bash
cd /data/projects
git clone http://172.25.45.244:13000/zhaoyufei/soujpg-dam-desktop.git damdesk-layout
cd damdesk-layout
git -c http.proxy= fetch gitea main

# ⚠️ 前置：UI-DIALOG 分支必须先合并（本计划复用它的 modal.tsx）
git merge --no-ff gitea/wt/t_9642d1d1 -m "merge UI-DIALOG 基座（modal.tsx）"
ls web/src/lib/modal.tsx   # 期望存在

git worktree add /data/projects/soujpg-dam-desktop-wt/LAYOUT2 -b wt/LAYOUT2 gitea/main
cd /data/projects/soujpg-dam-desktop-wt/LAYOUT2

# 独占容器（不要用别人在跑的 dam-dev / dam-c9-build 等）
docker run -d --name dam-layout2-build \
  -v /data/projects/soujpg-dam-desktop-wt/LAYOUT2:/workspace \
  -w /workspace damdesk-build:latest sleep infinity   # 镜像名按本机 `docker images` 里既有的 rust+node 构建镜像替换
docker exec dam-layout2-build bash -lc 'cd /workspace/web && pnpm install --frozen-lockfile'

# 基线（必须全绿）
docker exec dam-layout2-build bash -lc 'cd /workspace/web && npx tsc -b --pretty false'   # 期望：无输出
docker exec dam-layout2-build bash -lc 'cd /workspace/web && pnpm vitest run'              # 期望：全 passed
```

---

## 7. Step-by-step 任务

> 每个任务固定循环：写失败测试 → 跑（红）→ 最小实现 → 跑（绿）→ `tsc -b` + 全量 vitest → commit。
> 统一测试命令：`docker exec dam-layout2-build bash -lc 'cd /workspace/web && pnpm vitest run tests/<文件>'`

### Stage A — Dialog 基础设施

#### Task A1：URL 驱动的 Dialog 开关 hook

**测试** `web/tests/dialog-param.test.tsx`
```tsx
import { renderHook, act } from '@testing-library/react'
import { MemoryRouter } from 'react-router-dom'
import { describe, expect, it } from 'vitest'
import { useDialogParam } from '../src/lib/useDialogParam'

const wrap = (init: string) => ({ wrapper: ({ children }: any) => <MemoryRouter initialEntries={[init]}>{children}</MemoryRouter> })

describe('useDialogParam', () => {
  it('从 ?dialog=settings 读到 settings', () => {
    const { result } = renderHook(() => useDialogParam('settings'), wrap('/?dialog=settings'))
    expect(result.current.open).toBe(true)
  })
  it('openDialog 写入参数、closeDialog 删除参数', () => {
    const { result } = renderHook(() => useDialogParam('settings'), wrap('/'))
    expect(result.current.open).toBe(false)
    act(() => result.current.openDialog())
    expect(result.current.open).toBe(true)
    act(() => result.current.closeDialog())
    expect(result.current.open).toBe(false)
  })
})
```
**实现** `web/src/lib/useDialogParam.ts`
```ts
import { useCallback } from 'react'
import { useSearchParams } from 'react-router-dom'

export function useDialogParam(key: string) {
  const [params, setParams] = useSearchParams()
  const open = params.get('dialog') === key
  const openDialog = useCallback(() => {
    const next = new URLSearchParams(params)
    next.set('dialog', key)
    setParams(next, { replace: false })
  }, [params, key, setParams])
  const closeDialog = useCallback(() => {
    const next = new URLSearchParams(params)
    next.delete('dialog')
    setParams(next, { replace: true })
  }, [params, setParams])
  return { open, openDialog, closeDialog }
}
```
**验证**：2 passed。**commit** `feat(layout): URL 驱动 Dialog 开关 hook`。

#### Task A2：通用 Dialog 外壳 `components/AppDialog.tsx`

**测试** `web/tests/app-dialog.dom.test.tsx`：断言 ①渲染标题与 ✕；②Esc 关闭；③点遮罩关闭、点内容不关；④带 `max-w-4xl`（宽档可选 `sm|md|lg|xl`）。
**实现**（复用 `lib/modal.tsx`）
```tsx
import { ReactNode } from 'react'
import { useModalDismiss, ModalCloseButton } from '../lib/modal'

const WIDTH = { sm: 'max-w-md', md: 'max-w-2xl', lg: 'max-w-4xl', xl: 'max-w-6xl' } as const

export function AppDialog({
  open, onClose, title, children, width = 'lg', footer, testId,
}: {
  open: boolean; onClose: () => void; title: string
  children: ReactNode; width?: keyof typeof WIDTH; footer?: ReactNode; testId?: string
}) {
  const { overlayProps, contentRef } = useModalDismiss<HTMLDivElement>({ open, onClose })
  if (!open) return null
  return (
    <div {...overlayProps} className="fixed inset-0 z-40 flex items-center justify-center bg-black/50 p-6">
      <div ref={contentRef} role="dialog" aria-modal="true" aria-label={title} data-testid={testId ?? 'app-dialog'}
           className={`flex max-h-[88vh] w-full ${WIDTH[width]} flex-col overflow-hidden rounded-lg border border-line bg-panel shadow-2xl`}>
        <header className="flex shrink-0 items-center gap-2 border-b border-line px-4 py-3">
          <h2 className="text-sm font-semibold">{title}</h2>
          <div className="ml-auto"><ModalCloseButton onClose={onClose} label={`关闭${title}`} /></div>
        </header>
        <div className="min-h-0 flex-1 overflow-y-auto p-4">{children}</div>
        {footer && <footer className="shrink-0 border-t border-line px-4 py-3">{footer}</footer>}
      </div>
    </div>
  )
}
```
**验证**：4 断言 passed。**commit** `feat(layout): 通用 AppDialog 外壳`。

---

### Stage B — 低频功能 Dialog 化（每项一个任务）

#### Task B1：设置 → `SettingsDialog`
- 新建 `web/src/components/dialogs/SettingsDialog.tsx`：**用 Drawer 而不是 modal**（按 `soujpg-ui-patterns` §9.2.1：5 个分区、字段 >6、需要滚动 → 升级载体；右侧滑出、宽度 560px、可滚动，保留背景可比对）。内部左侧分区 tab（推理引擎 / AI 打标 / 合规 / 存储与清理 / 关于），右侧放**原 `pages/Settings.tsx` 的对应 JSX 段**（整段搬移，不改任何请求与字段名）。Drawer 同样必须 Esc / ✕ / 点遮罩关闭 + 焦点归还（复用 `web/src/lib/modal.tsx`，只是把面板从居中改为右侧滑出）。
- `pages/Settings.tsx` 处理：内容全部迁走后该类文件保留为一个 `SettingsDialog` 的薄包装（避免其他代码 import 断裂）；路由里 `{ path: 'settings', element: <Navigate to="/?dialog=settings" replace /> }`。
- `App.tsx` 顶栏加 `⚙` 按钮（`aria-label="设置"`）调用 `openDialog()`；`<SettingsDialog open={open} onClose={closeDialog} />` 挂在根节点。
- **测试** `web/tests/settings-dialog.dom.test.tsx`：①点顶栏 ⚙ 出现 `role=dialog` 名「设置」；②分区 tab 点击切换面板；③Esc 关闭后 URL 无 `dialog` 参数；④旧路由 `/settings` 重定向后对话框自动打开。
- **验证**：4 断言 passed；`grep -c "api\." pages/Settings.tsx` 与迁移后 SettingsDialog 内 `api.` 调用数一致（证明没丢表单字段）：期望两边相等。
- **commit** `feat(layout): 设置改为顶栏按钮 + Dialog（分区 tab）`。

#### Task B2：导出 → `ExportDialog`
- 新建 `web/src/components/dialogs/ExportDialog.tsx`：`AppDialog width="lg"`，**单面板无 tab**（用户裁决：导出是基于选择的一次性动作，不做"导出历史"概念）。内容 = 原 `pages/ExportPage.tsx` 的表单（内容类型 / 目标目录或 zip / manifest）。
- 顶栏 `导出…` 按钮打开；图库选中集导出默认范围 = 选择集（`selection.ts`），无选择集时 = 当前筛选。
- 路由 `/export` → `<Navigate to="/?dialog=export" replace />`。
- **测试** `web/tests/export-dialog.dom.test.tsx`：①顶栏按钮打开；②默认范围文案含"已选 N 张"或"当前筛选"（有选择集时用选择集，无则用筛选）；③点「开始导出」调用 `api.exportStart`（mock 后断言被调用一次，且范围参数与②一致）；④Esc 关闭；⑤**断言不存在"历史"tab/文案**（`queryByText(/历史/)` 为 null）。
- **commit** `feat(layout): 导出改为 Dialog（默认范围=选择集/筛选）`。

#### Task B3：行业包管理 → `PackManagerDialog`
- 现 `web/src/components/PackManager.tsx` 已是组件；新建 `components/dialogs/PackManagerDialog.tsx` 包裹它（`AppDialog width="lg"`），`pages/Compliance.tsx` 的「行业包管理 →」改为按钮 `openDialog()`。
- **测试** `web/tests/pack-dialog.dom.test.tsx`：点「行业包管理」出现 dialog、包列表非空、Esc 关闭。
- **commit** `feat(layout): 行业包管理改 Dialog`。

#### Task B4：自定义打标任务 → `CustomTaskDialog`
- 新建 `components/dialogs/CustomTaskDialog.tsx`：表单（任务名 / prompt / 作用范围），提交调已有 `api`（先 `grep -n "custom" web/src/lib/api.ts` 确认方法名，预期 `customTask*` 系列）。
- `pages/TagTasks.tsx` 的「＋ 新建任务」改为打开该 Dialog；每行「编辑」也复用（传 `initial`）。
- **测试** `web/tests/custom-task-dialog.dom.test.tsx`：①空任务名提交被拦（错误文案）；②合法提交调用 API 一次；③Esc 关闭不提交。
- **commit** `feat(layout): 自定义打标任务改 Dialog`。

#### Task B5：回收站 → `RecycleBinDialog`
- 把 `pages/Dedup.tsx` 里「回收站（0）展开 ▼」那段内联列表搬进 `components/dialogs/RecycleBinDialog.tsx`（`AppDialog width="md"`），去重页改为「回收站 (N)」按钮。
- **测试** `web/tests/recycle-dialog.dom.test.tsx`：①按钮显示计数；②打开后列出被软删项；③「恢复」调用 `api.restoreAssets`（名字以 `grep -n "restore\|softDelete" web/src/lib/api.ts` 结果为准）。
- **commit** `feat(layout): 回收站改 Dialog`。

#### Task B6：失联核查详情 → `MissingDialog`
- `LibraryTree.tsx` 的「失联 N」行加「明细」按钮 → `components/dialogs/MissingDialog.tsx`：列出 missing/notFound 的路径与原因（数据来自 `api.verifyMissing` 或 `api.stats`，按实际返回字段）。
- **测试** `web/tests/missing-dialog.dom.test.tsx`：mock 返回 3 条 → 表格渲染 3 行；空数据显示「无失联文件」。
- **commit** `feat(layout): 失联核查明细改 Dialog`。


#### Task B7：后端补「按资产集合打标」参数
（用户 2026-10-17 追加裁决：处理页要能"基于过滤条件圈一批图"创建任务。合规侧已支持，打标侧还不支持。）

现状（已核实，勿重复侦察）：
- `POST /api/compliance/judge` 的 `JudgeReq` **已支持** `assetIds: Option<Vec<i64>>` / `all` / `limit` / `scope`（`crates/app-web/src/compliance_api.rs` L629-647）→ 合规 tab 直接可用。
- `POST /api/tag/start` 的 `StartReq` 只有 `mode: pending|all` + `limit`（`crates/app-web/src/tag_api.rs` L66-73）；`POST /api/tag/tasks/{id}/run` 的 `TaskRunReq` 同样只有 `mode` + `limit` → **无法只打某批图**。

任务：
1. `StartReq` / `TaskRunReq` 各加 `#[serde(default)] pub assetIds: Option<Vec<i64>>`；语义：给了 `assetIds` 就忽略 `mode` 的取库逻辑，只对这批 id 入队（仍尊重 `limit`）；空数组 = 400 `empty_scope`。
2. 入队前校验 id 存在且 `deleted = 0`，不存在的 id 记入响应 `skippedInvalid: n`（不要静默丢）。
3. 响应体加 `enqueued: n` / `scope: "explicit"|"pending"|"all"` 便于前端回显。
4. 测试 `crates/app-web/tests/tag_scope.rs`（新建）：①只传 assetIds 时仅这几张入队（断言 job_queue 行数 = N 且 targetId 集合相等）；②空数组 400；③不存在 id 进 skippedInvalid；④不传 assetIds 时行为与现在完全一致（回归）。
5. **commit** `feat(tag): /api/tag/start 与 tasks/{id}/run 支持 assetIds 范围打标`。

---

### Stage C — 区域化与主视图收敛

#### Task C1：图库内联搜索（吸收 `/search`）
- 图库工具条（`pages/Gallery.tsx` 顶部区）加搜索框 + 模式选择 `select`：`文件名/标题`（走 `api.assets({q})`）、`语义`（走 `api.searchText`，先 `grep -n "searchText\|textSearch\|/api/search/text" web/src/lib/api.ts` 确认）、`以图搜图`（文件选择 + `api.searchImage`，同法确认）。
- 语义/图搜结果直接渲染到同一网格（复用现有 `AssetGrid`），并在结果区顶部显示「语义搜索：雪山 · 12 条 · 清除」。
- `/search` 路由改为 `<Navigate to="/" replace />`（保留 `pages/Search.tsx` 文件一段时间不动，避免 import 断裂，注释标记"待删除"）。
- **测试** `web/tests/gallery-inline-search.dom.test.tsx`：①模式切换后 placeholder 变化；②文件名模式输入回车 → 调 `api.assets` 且 query 含 `q`；③语义模式请求 `/api/search/text`（fetch mock 断言 URL）；④清除按钮复原网格。
- **commit** `feat(layout): 搜索内联进图库（三模式），/search 重定向`。

#### Task C2：导航与顶栏重排
- `App.tsx`：`NAV_GROUPS` 改为两组 —— **浏览**（图库 `/`、去重 `/dedup`）、**处理**（处理 `/tag-tasks`，即打标+合规合页）；侧栏宽 `w-40`（160px）。
- 处理页改造：新建 `web/src/pages/Process.tsx`，内部 `useState<'tag'|'compliance'>` 或读 `?tab=`，两个 tab 分别渲染现有 `pages/TagTasks.tsx`、`pages/Compliance.tsx` 的组件（**整页搬移，不改内部逻辑**）；`/tag-tasks`、`/compliance` 两条路由都保留，分别映射到对应默认 tab（书签不破），`usePageTitle()` 统一显示「处理」。
- 顶栏右侧按钮顺序：`＋ 导入`、`导出…`、`⚙`、任务状态；任务入口从顶栏移到侧栏底部（「进行中 N」，`TaskCenterSummary`）。
- `usePageTitle()` 从两组里查找；`/tag-review` 不进导航。
- **测试** `web/tests/nav-zones.dom.test.tsx`：①导航只有 **3** 个链接（图库/去重/处理）；②分组标题「浏览」「处理」存在；③顶栏按顺序有 `导入/导出/设置` 三个按钮的 `aria-label`；④`/tag-review` 不在导航中；⑤访问 `/compliance` 时处理页默认停在「合规」tab。
- **commit** `feat(layout): 导航收敛为 浏览/处理 两区 + 顶栏动作区`。

#### Task C3：图库侧栏只留高频三段
- `LibraryTree.tsx`：**整段删除「合规（包 xxx）」**（用户裁决：判定属处理页，图库不出现合规内容）。保留 库 / 目录 / 标签 / 评分，并给每段加：折叠箭头 + 段内 🔍 过滤输入（新建 `components/CollapsibleSection.tsx`）。
- 图库工具条**也不加**「判定」下拉（用户裁决：处理相关只放处理界面）。
- 替代路径：图库工具条放一个中性的「处理 →」链接指向 `/tag-tasks`（不携带任何合规筛选）。
- **测试** `web/tests/library-tree-sections.dom.test.tsx`：①图库侧栏与工具条**都不存在**「合规/待人工/合格/判定」任何文案（`queryByText(/合规|判定|待人工/)` 全 null）；②三段可折叠（点击标题后条目消失）；③标签段输入过滤生效；④「处理 →」链接 href 为 `/tag-tasks`。
- **commit** `feat(layout): 图库侧栏收敛为高频三段 + 折叠/过滤`。

#### Task C4：检查器空态收起 + 侧栏折叠 + 断点
- `Inspector.tsx`：未选中时 `w-10` 竖条（点击展开），选中时 `w-72`（详见 3.现状证据 L88-95）。
- 布局偏好模块 `web/src/lib/layoutPrefs.ts`（`navCollapsed/treeCollapsed/treeWidth/inspectorOpen`，key `dam.layout`，坏 JSON 回退默认）。
- `App.tsx` 侧栏可折叠（`w-40 ↔ w-14`）；`Gallery.tsx` 侧栏折叠与宽度拖动（200–480）。
- 断点：`useMediaQuery`（`max-width: 1023px` → 导航图标条；`max-width: 1279px` → 图库侧栏变抽屉）。
- **测试** `web/tests/layout-prefs.test.ts`（3 例）+ `web/tests/inspector-collapse.dom.test.tsx`（2 例）+ `web/tests/layout-breakpoints.dom.test.tsx`（3 例）。
- **commit** `feat(layout): 折叠/宽度/断点（检查器空态收起、导航图标化、侧栏抽屉）`。


#### Task C5：共享「范围选择器」RangePicker（处理页两个 tab 共用）
（用户裁决：点打标页也要能基于过滤条件选一批图创建任务。）

组件 `web/src/components/RangePicker.tsx`，受控 props `{ value, onChange }`，`value` 三种形态：
```ts
export type Range =
  | { kind: 'all' }                                            // 全库（deleted=0）
  | { kind: 'selection' }                                      // 图库当前选择集（lib/selection.ts）
  | { kind: 'filter'; query: AssetQuery }                      // 复用 lib/api.ts 的 AssetQuery（目录/标签/评分/日期/未打标…）
```
行为：
- 三选一 radio：「全部图片」「图库已选 N 张」（无选择集时禁用并给提示「先在图库选中图片」）「按筛选条件…」
- 选「按筛选条件」时展开与图库一致的条件控件（复用 `LibraryTree` 已有的目录/标签/评分数据源与 `AssetQuery` 字段，**不要另写一套**）
- **实时预估命中数**：调 `api.assets(query)` 取 `total`，显示「命中 N 张」；N=0 禁用主按钮；N>500 时提示「范围较大，建议先重判/先打标」（不阻断）
- 提交时把 Range **解析成 id 快照**（`api.assets(...).items.map(a => a.assetId)`），同时把条件原文序列化存进任务/判定的 meta，UI 回显「本次范围：标签=狗 且 评分≥4（快照 12 张）」
- 快照理由：动态条件会在执行期间随新增图/标签变化而漂移，任务结果不可复现。

接入：
- 「处理」页「打标」tab：`增量打标`/`全库重打` 按钮上方放 RangePicker，提交走 Task B7 的新 `assetIds` 参数。
- 「处理」页「合规」tab：`开始判定` 上方放同一个 RangePicker，提交走已有 `JudgeReq.assetIds`。

测试 `web/tests/range-picker.dom.test.tsx`：①默认 all 显示命中数；②切换 selection 无选择集时禁用；③filter 模式改标签后命中数变化（mock `api.assets`）；④提交回调收到的 Range 正确。
**commit** `feat(process): 共享范围选择器（全部/选择集/按筛选）接入打标与合规`。

#### Task C6：图库 → 处理的带上下文跳转
- `web/src/components/Inspector.tsx` 多选态加按钮「送去处理」（`aria-label="送去处理"`）→ `navigate('/tag-tasks?ids=' + [...selection].join(','))`
- 「处理」页读 `?ids=`：解析成选择集快照，RangePicker 显示为「来自图库已选 N 张（清除）」；点「清除」回落到 `all`
- 顶栏「导出…」同样接受当前选择集（Task B2 已实现范围默认值）
- 测试 `web/tests/gallery-to-process.dom.test.tsx`：①选中 3 张点「送去处理」→ URL 含 `ids=` 且处理页 RangePicker 显示 3 张；②点「清除」→ 变回全部。
- **commit** `feat(process): 图库选中集带上下文跳处理页`


**打标任务的统一入口（2026-10-17 用户追加：任何地方选出的图片集都能建任务）**

所有来源最终都走同一条路：**选出集合 → RangePicker 预填 → 点明确的动作按钮 → `POST /api/tag/start {assetIds, template}`**。不同来源只决定"预填范围"，不各自实现打标逻辑。

| 来源 | 入口 | 预填范围 |
|---|---|---|
| 图库多选 | 检查器「送去打标」 | 选择集 id 快照 |
| 图库当前筛选（如 标签=狗 4 张） | 图库工具条「按当前筛选打标」 | 当前 `AssetQuery` |
| 搜索 / 语义结果 | 结果区「对 N 条结果打标」 | 结果 id 快照 |
| 智能文件夹 | 文件夹页「打标此文件夹」 | 该文件夹 query |
| 合规「不合格」档 | 合规 tab「对不合格项重新打标」 | 该档 id 快照 |
| 处理页自选 | RangePicker（全部/选择集/筛选） | 用户选择 |

**硬性约束（不要省）**
1. **选择 ≠ 创建**：选中图片只决定范围，必须再点一次动作按钮；打标是花钱的异步作业（DeepSeek 按量计费），不允许"选中即自动建任务"。
2. 动作前必须有确认闸：弹确认框显示「将处理 N 张 · 模型 provider:model · 模板：三合一 / 自定义任务名」，N 与模型都要来自真实预估值。
3. 幂等：同一 `assetIds + 模板` 在 30 秒内重复提交返回同一 `batchId`，不重复入队（防止连点）。
4. 建任务后进任务中心（侧栏底部「进行中 N」），可取消/续跑；范围快照存进任务 meta，UI 回显条件原文。

---

### Stage D — 收尾

#### Task D1：命令面板补入口与动作
- `components/CommandPalette.tsx`：动作列表加「导出…」「设置」「回收站」「行业包管理」（调用各自 `openDialog()`），保证 Dialog 化后仍可从键盘到。**测试** `web/tests/palette-dialogs.dom.test.tsx`：输入"设置"回车 → `role=dialog` 名「设置」出现。
- **commit** `feat(layout): 命令面板补齐 Dialog 动作`。

#### Task D2：快捷键帮助与文档同步
- `ShortcutsHelp.tsx` 的快捷键表补：`⌘K` 命令面板、`?` 帮助、`Esc` 关弹窗、`Delete` 删除、`E` 导出选择集（按现有实现核对，不写不存在的键）。
- `web/src/App.tsx` 顶部注释写清分区结构（浏览/处理 + 顶栏动作 + Dialog 清单），便于后人维护。
- **commit** `docs(layout): 分区结构与快捷键表同步`。

---

## 8. 验证 / 里程碑验收

```bash
# 每任务收尾
docker exec dam-layout2-build bash -lc 'cd /workspace/web && npx tsc -b --pretty false && pnpm vitest run && pnpm vite build'
# 期望：tsc 无输出；vitest 全 passed；vite 产出 dist/ 且 exit 0
```

**量化验收（CDP，本地 dam-web 实例，不动真机）**

| # | 验收项 | 判定方法 | 期望 |
|---|---|---|---|
| 1 | 导航项数量 | `document.querySelectorAll('nav a').length` | 4 |
| 2 | 顶栏动作 | 三个按钮 `aria-label` | 导入 / 导出 / 设置 |
| 3 | 固定占用（1440 视口，未选中资产） | `aside.offsetWidth + tree.offsetWidth + inspector.offsetWidth` | ≤ 480 |
| 4 | 缩略图列数（1280 视口） | 网格 `clientWidth / 单卡宽` | ≥ 4 |
| 5 | 无横向滚动（1000 视口） | `document.documentElement.scrollWidth <= innerWidth` | true |
| 6 | 低频入口全部为 Dialog | 依次点 导出…/⚙/行业包管理/新建任务/回收站/失联明细 | 每个都出现 `role=dialog`，且 Esc 可关 |
| 7 | 旧路由兼容 | 访问 `/settings`、`/export`、`/search` | 分别重定向并自动打开对应 Dialog / 回到图库 |
| 8 | 搜索内联 | 图库工具条输入「狗」回车 | 网格筛到含「狗」标签的图；语义模式请求 `/api/search/text` |
| 9 | 回归 | `pnpm vitest run` | 全 passed（含既有 35+ 用例） |

---

## 9. 风险 / 权衡 / 开放问题

**风险**
- **与并行卡冲突**：`t_cef05116`（合规页）、`t_bf8f7e95`（图库删除/筛选横幅）正在改 `web/src`；本计划的 Stage B1/B2/C3 与它们同文件。**开工前必须确认这两张已合并**（`git log --oneline gitea/main -8 | grep -E "cef05116|bf8f7e95"`），否则先只做 `web/src/lib/*`、`components/dialogs/*`、`components/AppDialog.tsx` 这些新文件。
- 设置/导出内容整体搬进 Dialog 时容易漏搬字段：用「搬前 `api.` 调用数 == 搬后 `api.` 调用数」做机械校验（Task B1/B2 已写）。
- Dialog 内长表单在 88vh 高度下需要内部滚动：`AppDialog` 已给 `overflow-y-auto`，但**不要**在内部再嵌 `overflow-y-auto` 二级滚动，否则触控板滚不动（真机验证时必须试）。
- 桌面壳（Tauri）与 web 形态共用同一份 `web/`：`以图搜图` 在 web 形态需服务端路径，`api.searchImage` 已支持字节上传（`POST /api/search/image`），两种形态都可用；拖拽导入不在本计划范围。

**权衡**
- 不做「工作区式自由布局/可拖拽面板」（YAGNI，且与并行卡冲突风险高）。
- 保留 `pages/Search.tsx`、`pages/Settings.tsx`、`pages/ExportPage.tsx` 文件不删（先重定向或薄包装），避免一次性大删除引发 import 断裂；清理留到后续卡。
- 导航从 7 项砍到 3 项（图库/去重/处理）是**信息架构改动**；打标与合规合并进「处理」页两个 tab，两条主线都在，不会丢失入口。

**已裁决（2026-10-17 用户，权威）**
1. 打标 + 合规 → **合并为「处理」页的两个 tab**（导航 3 项）。
2. 导出 **不做"历史"tab**：导出就是"基于用户选择（或当前筛选）导出一次"的动作，Dialog 单面板。
3. 判定/合规相关的一切（筛选、统计、复核）**只放在处理页的「合规」tab**，图库页彻底不出现（含工具条下拉也不加）。

**仍开放（需产品裁决，实现者不要自定）**
- 处理页的 tab 顺序与默认 tab（当前默认：从 `/tag-tasks` 进=打标 tab，从 `/compliance` 进=合规 tab）。
- 设置 Dialog 内「关于」是否显示版本号/模型清单（取决于后端有无 `/api/status` 类接口，实现前 `grep` 确认）。
