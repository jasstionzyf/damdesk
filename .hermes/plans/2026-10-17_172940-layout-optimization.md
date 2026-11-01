# 布局优化计划（基于产品功能）

- 计划文件：`.hermes/plans/2026-10-17_172940-layout-optimization.md`
- 目标仓库：`http://172.25.45.244:13000/zhaoyufei/soujpg-dam-desktop.git`（分支 `main`，本计划基线 `5677971` 之后）
- 适用形态：Tauri 桌面壳 与 web 形态共用同一套 `web/` 代码（`crates/app-web` 提供 API）
- 面向读者：没有任何仓库上下文的实现者

---

## 1. Goal（一句话）

在 **不新增产品功能** 的前提下，把现有 7 个功能页（图库 / 搜索 / 去重 / 打标任务 / 合规 / 导出 / 设置）的布局重排为「固定占用更小、常用工作流少跳页、窄窗可用」的结构：把固定横向占用从 **736px 降到 ~470px**，让 1280px 宽的窗口仍能放下 ≥4 列缩略图，并消除两套进度视图与两处合规入口的语义重复。

---

## 2. 现状与证据（已实测，勿重新侦察）

### 2.1 当前布局骨架（三处文件）

| 区域 | 文件 | 事实 |
|---|---|---|
| 主导航侧栏 | `web/src/App.tsx` L118 `<aside className="flex w-52 shrink-0 …">` | 固定 **208px**，含 7 个 `NavLink`（`NAV` 常量 L14-22）+ 智能文件夹 + 底部「命令面板 ⌘K」小按钮 |
| 顶栏 | `web/src/App.tsx` L146-160 `<header className="flex h-12 …">` | 高 48px，只有 左侧页面标题、右侧「＋ 导入」「☰ 任务」；**没有任何搜索输入**（`grep placeholder` 计数 0） |
| 图库二级侧栏 | `web/src/components/LibraryTree.tsx` L266 `<div className="flex h-full w-60 shrink-0 …">` | 固定 **240px**：库（全部/最近/未打标/失联/重新核查）→ 合规分组（待人工/不合格/修复后合格/合格）→ 目录 → 标签 → 评分；每段无折叠、无段内搜索 |
| 检查器 | `web/src/components/Inspector.tsx` L90 `<div className="flex h-full w-72 shrink-0 …">` | 固定 **288px**，**未选中任何图时也占满 288px** 只显示「未选中图片」 |
| 图库主区 | `web/src/pages/Gallery.tsx` L961 `<div className="flex min-h-0 flex-1">` | 上述四块拼装处 |

固定占用合计 = 208 + 240 + 288 = **736px**。1440px 窗口下缩略图区仅 ~704px；1280px 下 ~544px；1024px 下 ~288px（基本不可用）。

### 2.2 响应式几乎是空的

`grep -oE '\b(sm|md|lg|xl|2xl):'` 全仓只命中 5 处，分别在 `pages/Dedup.tsx`(2)、`pages/Settings.tsx`(1)、`pages/TagTasks.tsx`(2)。`App.tsx`、`Gallery.tsx`、`LibraryTree.tsx`、`Inspector.tsx` **零断点**。

### 2.3 现有信息架构的问题（按严重度）

- **P0-a 窄窗不可用**：见 2.1/2.2，无折叠、无断点、Inspector 空态也占位。
- **P0-b 搜索与图库割裂**：搜索是独立页 `/search`（`web/src/pages/Search.tsx`），用户搜到图后要批量打标/导出必须回图库重新找；顶栏没有搜索框，唯一快捷方式是 ⌘K（入口埋在侧栏最底部 12px 小字）。
- **P0-c 图库侧栏无折叠无段内搜索**：实测库内仅 27 张图就产生 30+ 个标签行（`#\n户外 4`、`#\n草地 4` …），`LibraryTree.tsx` L396 起整段 `overflow-y-auto` 顺序堆叠，真实库（万级图、几百标签）会淹没「目录/评分」。
- **P0-d 进度视图有两套**：顶栏「☰ 任务」打开 `TaskCenterDrawer`（`web/src/components/TaskCenter.tsx`），而打标任务页/合规页/导出页各自又有状态条（如 `pages/TagTasks.tsx` 的「打标引擎 / 队列 pending / done / failed」、`pages/Compliance.tsx` 的「上一轮判定完成 · 逐图/组级」）。同一件事两个地方看，哪个是权威不明确。
- **P1-e 导航混杂**：`NAV`（`App.tsx` L14）把工作台（图库/搜索/去重）、作业页（打标任务/合规）、动作页（导出）与配置（设置）平铺成 7 项同级，无分组、无分隔。
- **P1-f 合规两处入口不互链**：`/compliance` 页 与 `LibraryTree` 的「合规（包 realestate）」分组各有一套筛选，互不跳转（真机实测：合规页看不到合格图，只能回图库侧栏点「✓ 合格 15」——该缺口另卡 `t_cef05116` 在修，本计划只负责布局侧的入口互链）。
- **P1-g 设置页单页长表单**：`pages/Settings.tsx` 把 embedder、tag provider、合规参数（GPS/坐标）、清理与重置堆在一页（真机实测滚动深度 > 2 屏），无分区导航。
- **P1-h 打标任务页四块堆叠**：`pages/TagTasks.tsx` 依次是 引擎状态 / 模板打标 / OCR / 自定义打标任务，全是长段落 + 按钮组，无 tab。
- **P2-i 命令面板入口不可发现**：只在侧栏底部（`App.tsx` L163-171）。
- **P2-j 拖拽导入路径不明**：主界面（图库）没有任何 drop 区提示，`App.tsx` L63-68 只用 `dam:open-import` 事件打开向导；用户拖一批图到窗口没有反应。

---

## 3. Architecture / 方案概述

把「固定导航 + 固定二级侧栏 + 固定检查器」改成 **可折叠 + 可拖动 + 有断点** 的三区布局，并把搜索提升为顶栏全局入口（搜索结果落在图库网格，而不是独立页）：

1. **布局状态集中到一处**：在 `web/src/lib/persistedGallery.ts`（已有 `localStorage` 持久化模块，key `dam.gallery.filter`）新增布局偏好（`navCollapsed`、`treeCollapsed`、`inspectorOpen`、`treeWidth`），用同一套读写函数，避免各处各存。
2. **折叠优先于断点**：默认（≥1280px）保持三区，但 Inspector 未选中时收起为 40px 竖条、LibraryTree 可一键折叠；<1280px 时 LibraryTree 变为抽屉（浮层 + 遮罩），<1024px 主导航变图标条（宽 56px）。
3. **搜索内联**：顶栏新增搜索框，提交后 `navigate('/?q=…')`（或图库已有 `filter.q` 直接写入持久化状态），**不删** `/search` 页（页面级语义搜索仍保留），只把"找图"这条高频路径压到一跳。

> 说明：不动后端、不动 API、不改任何业务判定逻辑；纯前端布局与信息架构。

---

## 4. 环境准备（实现者从零开始）

```bash
# 1) 取仓库（首次）
cd /data/projects
git clone http://172.25.45.244:13000/zhaoyufei/soujpg-dam-desktop.git soujpg-dam-desktop-layout
cd soujpg-dam-desktop-layout

# 2) 建工作分支（本机 git 有 http.proxy 残留，push 要清）
git -c http.proxy= fetch gitea main
git worktree add /data/projects/soujpg-dam-desktop-wt/LAYOUT -b wt/LAYOUT gitea/main
cd /data/projects/soujpg-dam-desktop-wt/LAYOUT

# 3) 前端依赖（容器内，宿主无 node）
docker exec -it dam-dev bash -lc 'cd /workspace/web && pnpm install'

# 4) 基线验证（必须全绿再动手）
docker exec -it dam-dev bash -lc 'cd /workspace/web && npx tsc -b --pretty false'
# 期望：无输出，exit 0
docker exec -it dam-dev bash -lc 'cd /workspace/web && pnpm vitest run'
# 期望：Test Files / Tests 全 passed（当前基线 35 passed 起）
```

**注意**：容器内 `/workspace` 必须挂到本 worktree（`docker run -v /data/projects/soujpg-dam-desktop-wt/LAYOUT:/workspace …`）。若用既有 `dam-dev` 容器，请先 `docker exec dam-dev pwd` 确认它挂的是哪个目录，**不要**在别人正在用的容器里改代码（并行卡各自独占容器，命名 `dam-layout-build`）。

---

## 5. Step-by-step 任务

> 每个任务都按 TDD：先写失败测试 → 跑它确认失败 → 最小实现 → 跑它确认通过 → commit。
> 所有前端测试命令统一为 `docker exec dam-layout-build bash -lc 'cd /workspace/web && pnpm vitest run <文件>'`。

### Task 1（P0-a-1）：布局偏好的持久化模块

**测试** `web/tests/layout.prefs.test.ts`（新建）：

```ts
import { describe, expect, it, beforeEach } from 'vitest'
import { readLayout, writeLayout, DEFAULT_LAYOUT } from '../src/lib/layoutPrefs'

describe('layoutPrefs', () => {
  beforeEach(() => localStorage.clear())

  it('无存储时返回默认值', () => {
    expect(readLayout()).toEqual(DEFAULT_LAYOUT)
  })

  it('写入后能读回', () => {
    writeLayout({ ...DEFAULT_LAYOUT, treeCollapsed: true, inspectorOpen: false })
    expect(readLayout().treeCollapsed).toBe(true)
    expect(readLayout().inspectorOpen).toBe(false)
  })

  it('坏 JSON 回退默认值且不抛错', () => {
    localStorage.setItem('dam.layout', '{oops')
    expect(readLayout()).toEqual(DEFAULT_LAYOUT)
  })
})
```

**跑测试确认失败**：`pnpm vitest run tests/layout.prefs.test.ts` → 期望 `Cannot find module '../src/lib/layoutPrefs'`。

**实现** `web/src/lib/layoutPrefs.ts`（新建）：

```ts
export type Layout = {
  navCollapsed: boolean
  treeCollapsed: boolean
  treeWidth: number
  inspectorOpen: boolean
}

export const DEFAULT_LAYOUT: Layout = {
  navCollapsed: false,
  treeCollapsed: false,
  treeWidth: 240,
  inspectorOpen: true,
}

const KEY = 'dam.layout'

export function readLayout(): Layout {
  try {
    const raw = localStorage.getItem(KEY)
    if (!raw) return DEFAULT_LAYOUT
    const v = JSON.parse(raw) as Partial<Layout>
    return {
      navCollapsed: typeof v.navCollapsed === 'boolean' ? v.navCollapsed : DEFAULT_LAYOUT.navCollapsed,
      treeCollapsed: typeof v.treeCollapsed === 'boolean' ? v.treeCollapsed : DEFAULT_LAYOUT.treeCollapsed,
      treeWidth: typeof v.treeWidth === 'number' && v.treeWidth >= 200 && v.treeWidth <= 480 ? v.treeWidth : DEFAULT_LAYOUT.treeWidth,
      inspectorOpen: typeof v.inspectorOpen === 'boolean' ? v.inspectorOpen : DEFAULT_LAYOUT.inspectorOpen,
    }
  } catch {
    return DEFAULT_LAYOUT
  }
}

export function writeLayout(next: Layout): void {
  try {
    localStorage.setItem(KEY, JSON.stringify(next))
  } catch {
    /* 隐私模式忽略 */
  }
}
```

**跑测试确认通过**：期望 3 passed。**commit**：`feat(layout): 布局偏好持久化模块（dam.layout）`。

---

### Task 2（P0-a-2）：Inspector 未选中时收起为竖条

**测试** `web/tests/inspector-collapse.dom.test.tsx`（新建）：

```tsx
import { render, screen, fireEvent } from '@testing-library/react'
import { describe, expect, it, vi } from 'vitest'
import { Inspector } from '../src/components/Inspector'

describe('Inspector 空态折叠', () => {
  it('未选中时渲染为 40px 竖条且点击展开提示', () => {
    const { container } = render(<Inspector selected={[]} onCollapse={vi.fn()} collapsed />)
    const root = container.firstElementChild as HTMLElement
    expect(root.className).toContain('w-10')
    expect(screen.getByText('检查器')).toBeInTheDocument()
  })

  it('选中后自动展开（collapsed 被忽略）', () => {
    const { container } = render(
      <Inspector selected={[{ assetId: 1, fileName: 'a.jpg' } as never]} onCollapse={vi.fn()} collapsed />,
    )
    expect((container.firstElementChild as HTMLElement).className).toContain('w-72')
  })
})
```

> 若 `Inspector` 现有 props 与本例不同（当前为 `asset` / `isBatch`），先按实际签名改测试的入参，再实现；**不要**为了迁就测试改坏现有用法（`pages/Gallery.tsx` 里的调用点必须同步更新）。

**实现要点**（`web/src/components/Inspector.tsx` L88-95 一带）：

```tsx
if (!asset) {
  return (
    <div className={collapsed ? 'flex h-full w-10 shrink-0 flex-col items-center border-l border-line bg-panel' : 'flex h-full w-72 shrink-0 flex-col border-l border-line bg-panel'}>
      {collapsed ? (
        <button type="button" title="展开检查器" onClick={onCollapse} className="py-3 text-xs text-ink-dim hover:text-ink">
          检查器
        </button>
      ) : (
        <>
          <div className="border-b border-line px-4 py-3 text-xs font-semibold tracking-wider text-ink-dim">检查器</div>
          <div className="flex flex-1 flex-col items-center justify-center gap-2 px-6 text-center">
            <span className="text-2xl text-ink-dim"> ☗ </span>
            <p className="text-sm text-ink-dim">未选中图片</p>
          </div>
        </>
      )}
    </div>
  )
}
```

**验证**：上述 2 测试 passed + `npx tsc -b` 0 error。**commit**：`feat(layout): 检查器空态折叠为 40px 竖条`。

---

### Task 3（P0-a-3）：LibraryTree 折叠按钮与宽度可调

**测试** `web/tests/library-tree-collapse.dom.test.tsx`：

```tsx
import { render, screen, fireEvent } from '@testing-library/react'
import { describe, expect, it, vi } from 'vitest'
import { LibraryTree } from '../src/components/LibraryTree'

describe('LibraryTree 折叠', () => {
  it('折叠时不渲染目录/标签分区', () => {
    render(<LibraryTree collapsed onToggle={vi.fn()} width={240} onWidthChange={vi.fn()} />)
    expect(screen.queryByText(/目录/)).toBeNull()
  })

  it('点击折叠按钮触发 onToggle', () => {
    render(<LibraryTree collapsed={false} onToggle={vi.fn()} width={240} onWidthChange={vi.fn()} />)
    fireEvent.click(screen.getByRole('button', { name: /收起|折叠/ }))
  })
})
```

**实现**：`LibraryTree.tsx` L266 根节点改为

```tsx
<div className="flex h-full shrink-0 flex-col border-r border-line bg-panel" style={{ width: collapsed ? 0 : width }}>
```

并在 `components/StateViews.tsx` 或 `Gallery.tsx` 工具条加一个「☰ 侧栏」按钮调用 `onToggle`；宽度用原生 `resize` 手柄（`<div onMouseDown={…} className="w-1 cursor-col-resize" />`，拖动时 `onWidthChange(clamp(200,480))`）。折叠时把 `treeCollapsed` 写回 `layoutPrefs`。

**验证**：2 测试 passed；真机/本地视觉：1280 宽窗口折叠后有 ≥5 列缩略图。**commit**：`feat(layout): 图库侧栏可折叠与宽度调整`。

---

### Task 4（P0-a-4）：响应式断点（<1280 抽屉 / <1024 图标导航）

**测试** `web/tests/layout-breakpoints.dom.test.tsx`：用 `window.matchMedia` mock 驱动，断言：

```tsx
it('宽度 <1024 时主导航只显示图标（含 aria-label）', () => {
  mockMatchMedia(1000)
  render(<App />) // 若 App 依赖 Router，用 MemoryRouter 包裹
  expect(screen.getAllByRole('link', { name: '图库' })[0]).toHaveAttribute('aria-label', '图库')
  expect(screen.queryByText('打标任务')).toBeNull() // 文案隐藏
})
```

**实现**：新增 `web/src/lib/useMediaQuery.ts`：

```ts
import { useEffect, useState } from 'react'

export function useMediaQuery(query: string): boolean {
  const [hit, setHit] = useState(() => typeof window !== 'undefined' && window.matchMedia(query).matches)
  useEffect(() => {
    const mq = window.matchMedia(query)
    const on = () => setHit(mq.matches)
    mq.addEventListener('change', on)
    return () => mq.removeEventListener('change', on)
  }, [query])
  return hit
}
```

- `App.tsx` L118：`const narrow = useMediaQuery('(max-width: 1023px)')`；`<aside className={narrow ? 'w-14' : 'w-52'}>`，`NavLink` 内文案 `{!narrow && item.label}`，给每个链接补 `aria-label={item.label}`。
- `Gallery.tsx`：`const drawer = useMediaQuery('(max-width: 1279px)')`；`drawer` 时 `<LibraryTree>` 渲染为浮层（`fixed inset-y-0 left-0 z-20 shadow-xl` + 半透明遮罩），默认关闭。

**验证**：3 条断点断言 passed；用 CDP 在 1000px 视口截图（`Emulation.setDeviceMetricsOverride` width=1000）确认无横向滚动：`document.documentElement.scrollWidth <= window.innerWidth`。**commit**：`feat(layout): 响应式断点（导航图标化 + 侧栏抽屉）`。

---

### Task 5（P0-b）：顶栏全局搜索框

**测试** `web/tests/topbar-search.dom.test.tsx`：

```tsx
it('顶栏搜索提交后写入图库筛选 q 并导航到图库', async () => {
  render(<MemoryRouter initialEntries={['/search']}><App /></MemoryRouter>)
  const input = screen.getByPlaceholderText(/搜索图片|搜索/)
  fireEvent.change(input, { target: { value: '雪山' } })
  fireEvent.submit(input.closest('form')!)
  expect(localStorage.getItem('dam.gallery.filter')).toContain('雪山')
  expect(screen.getByRole('heading', { name: '图库' })).toBeInTheDocument()
})
```

**实现**：`App.tsx` L146-160 的 `<header>` 内，在标题后插入：

```tsx
<form
  className="ml-4 flex min-w-0 flex-1 items-center"
  onSubmit={(e) => {
    e.preventDefault()
    const q = searchRef.current?.value?.trim() ?? ''
    const next = { ...readGalleryFilter(), q }
    writeGalleryFilter(next)          // 复用 web/src/lib/persistedGallery.ts 既有写入函数
    navigate('/')
  }}
>
  <input ref={searchRef} type="search" placeholder="搜索文件名 / 标题（⌘K 打开命令面板）"
         aria-label="搜索图片"
         className="w-full max-w-md rounded-md border border-line bg-panel-2 px-3 py-1.5 text-sm outline-none focus:border-accent" />
</form>
```

**验证**：测试 passed；本地实例手测：在图库输入「狗」回车 → 网格筛到 2 张（真机库有 `狗` 标签）。**commit**：`feat(layout): 顶栏全局搜索框（提交落到图库筛选）`。

---

### Task 6（P0-c）：LibraryTree 分区折叠 + 段内搜索

**测试** `web/tests/library-tree-sections.dom.test.tsx`：断言「目录」「标签」分区可折叠（点击标题后该分区条目消失）、段内搜索输入能过滤标签列表（输入 `医疗` 后只剩含「医疗」的标签行）。

**实现**：抽 `components/CollapsibleSection.tsx`（标题 + 折叠箭头 + 可选过滤输入），在 `LibraryTree.tsx` L396 起的「目录」「标签」「评分」三段使用；过滤用 `label.includes(kw)`，不改数据结构。

**验证**：3 条断言 passed；真机 27 张库（30+ 标签）实测：折叠「目录」后标签段一屏可见。**commit**：`feat(layout): 图库侧栏分区折叠与段内过滤`。

---

### Task 7（P1-e）：主导航分组

**测试** `web/tests/nav-groups.dom.test.tsx`：断言页面出现三个分组标题「工作台」「审核」「系统」，且「设置」位于「系统」组内（用 `within()` 断言）。

**实现**：`App.tsx` L14 的 `NAV` 改为：

```ts
const NAV_GROUPS: { title: string; items: { to: string; label: string; icon: string }[] }[] = [
  { title: '工作台', items: [
      { to: '/', label: '图库', icon: '▣' },
      { to: '/search', label: '搜索', icon: '⌕' },
      { to: '/dedup', label: '去重', icon: '⧉' } ] },
  { title: '审核', items: [
      { to: '/tag-tasks', label: '打标任务', icon: '⚙' },
      { to: '/compliance', label: '合规', icon: '⚑' },
      { to: '/export', label: '导出', icon: '⇩' } ] },
  { title: '系统', items: [ { to: '/settings', label: '设置', icon: '☰' } ] },
]
```

渲染时每组一段 `<p className="px-3 pt-3 pb-1 text-[11px] tracking-wider text-ink-dim">{title}</p>`，`narrow` 模式隐藏分组标题与文案。`usePageTitle()` 改为在 `NAV_GROUPS.flatMap(g => g.items)` 里查找。

**验证**：测试 passed + `tsc -b` 0 error；CDP 截图确认分组标题渲染。**commit**：`feat(layout): 主导航按 工作台/审核/系统 分组`。

---

### Task 8（P0-d）：任务中心与页面状态条统一

**测试** `web/tests/task-entry.dom.test.tsx`：断言顶栏不再有「☰ 任务」按钮，改为侧栏底部「进行中 N」状态条；点击后打开同一个 `TaskCenterDrawer`；当 `tasks.length === 0` 时状态条显示「无进行中任务」且不可点。

**实现**：
- `App.tsx`：删掉 header 里的 `<TaskCenterButton>`，把计数入口移到侧栏底部（`mt-auto` 区域内）：
  ```tsx
  <TaskCenterSummary onClick={() => setTaskCenterOpen(true)} />
  ```
  在 `components/TaskCenter.tsx` 新增导出 `TaskCenterSummary`，内部 `useTasks()` 取 `tasks.filter(t => t.status === 'running').length`。
- 各页已有状态条保留（它们是**该页的细粒度视图**），但在状态条首行加一句统一文案与链接：「全局长任务见左下角『进行中 N』」。改动文件：`pages/TagTasks.tsx`、`pages/Compliance.tsx`、`pages/ExportPage.tsx`。

**验证**：测试 passed；真机点一次导入 → 左下角计数从 0→1、抽屉里出现该任务。**commit**：`feat(layout): 任务入口合并为侧栏状态条`。

---

### Task 9（P1-f）：合规两入口互链

**测试** `web/tests/compliance-links.dom.test.tsx`：断言 `/compliance` 页有「在图库中查看（同一筛选）」链接（`href` 含 `compliance=`），且 `LibraryTree` 合规分组标题右侧有「合规页 →」链接指向 `/compliance`。

**实现**：`pages/Compliance.tsx` 统计区加链接（与 `t_cef05116` 卡的三档改动兼容，优先用「按状态跳图库」参数名，保持与 `library_api` 的 `compliance` 参数一致）；`LibraryTree.tsx` 合规分组标题（L340 一带）加同款链接。**不要**改筛选语义，只加跳转。

**验证**：2 断言 passed；真机点「在图库中查看」→ 落到图库且筛选生效（`/api/library/assets?compliance=…` 请求参数可见）。**commit**：`feat(layout): 合规页与图库侧栏互链`。

---

### Task 10（P1-g）：设置页分区导航

**测试** `web/tests/settings-sections.dom.test.tsx`：断言出现 4 个分区锚点「推理引擎」「AI 打标」「合规」「存储与清理」，点击「合规」后 `location.hash === '#compliance'` 且对应 `section` 可见。

**实现**：`pages/Settings.tsx` 顶部加左侧窄栏（`<nav className="w-40 shrink-0">` + 4 个 `<a href="#…">`），现有 `<section>` 加 `id`；内容不动（只做锚点 + 滚动）。**不拆组件**（YAGNI，先解决可发现性）。

**验证**：测试 passed；真机滚动到「合规」区一次点击可达。**commit**：`feat(layout): 设置页分区锚点导航`。

---

### Task 11（P1-h）：打标任务页 tab 化

**测试** `web/tests/tagtasks-tabs.dom.test.tsx`：断言默认显示「引擎与队列」tab；点「OCR」「自定义任务」后对应面板出现且另外两个 `hidden`。

**实现**：`pages/TagTasks.tsx` 顶部加 `useState<'engine'|'ocr'|'custom'>('engine')` 的 tab 条；把现有四段包成三个面板（引擎状态 + 模板打标合并到 `engine`）。只移动 JSX、不改逻辑与请求。**验证**：测试 passed，`tsc -b` 0 error。**commit**：`feat(layout): 打标任务页 tab 化`。

---

### Task 12（P2-i）：命令面板入口提到顶栏

**实现**：`App.tsx` 顶栏右侧加 `⌘K` 样式的按钮（复用侧栏底部那个按钮的样式），侧栏底部保留（冗余可接受，不做删除）。**验证**：`grep -n "命令面板" web/src/App.tsx` 命中 2 处；测试：顶栏按钮点击后 `paletteOpen` 面板出现（`role=dialog` 名「命令面板」）。**commit**：`feat(layout): 顶栏命令面板入口`。

---

### Task 13（P2-j）：主界面整窗拖拽导入

**测试** `web/tests/window-drop.dom.test.tsx`：模拟 `window` 上 `dragover`/`drop` 事件（带 `DataTransfer.files`），断言出现全屏遮罩提示「松手导入这些文件」并调用 `dam:open-import`（用 `vi.spyOn(window, 'dispatchEvent')` 或断言向导 `role=dialog` 名「首启导入向导」出现）。

**实现**：新建 `web/src/components/DropOverlay.tsx`：监听 `window` 的 `dragenter`/`dragleave`/`drop`（`drop` 时 `e.preventDefault()`、取出 `files` 路径 —— 桌面壳走 `lib/native.ts` 的既有能力；web 形态提示"网页版请用导入向导选择服务器路径"），渲染 `fixed inset-0 z-50` 遮罩。挂到 `App.tsx` 根节点内。

**验证**：测试 passed；真机拖一张图到窗口 → 出现遮罩并打开导入向导。**commit**：`feat(layout): 整窗拖拽导入入口`。

---

## 6. Tests / validation（每个任务的固定循环）

```bash
# 1) 先写测试 → 跑（应失败）
docker exec dam-layout-build bash -lc 'cd /workspace/web && pnpm vitest run tests/<新测试文件>'
# 期望：FAIL，错误信息为模块/元素不存在

# 2) 最小实现后再跑（应通过）
docker exec dam-layout-build bash -lc 'cd /workspace/web && pnpm vitest run tests/<新测试文件>'
# 期望：Tests  N passed (N failed=0)

# 3) 全量回归 + 类型 + 构建（每个任务收尾都跑）
docker exec dam-layout-build bash -lc 'cd /workspace/web && npx tsc -b --pretty false && pnpm vitest run && pnpm vite build'
# 期望：tsc 无输出；vitest 全 passed；vite 输出 dist/ 且 exit 0

# 4) 视觉/交互验收（CDP，本地 dam-web 形态，不要动真机）
#    在容器里起 web 形态：pnpm vite build && 由 crates/app-web 提供静态目录；或直接用既有 dam-web 容器
python3 - <<'PY'
# 用 skill 提供的客户端：~/.hermes/profiles/soujpg/skills/projects/soujpg-dam-desktop/scripts/desktop-e2e/windows/cdp_client.py
# 断言（示例）：
#  a) 1440 视口：固定占用 = aside + tree + inspector 的实际 offsetWidth 之和 <= 500
#  b) 1000 视口：document.documentElement.scrollWidth <= window.innerWidth
#  c) 折叠 tree 后：document.querySelectorAll('[data-testid=asset-grid] img').length >= 5
PY
```

**里程碑验收（全部任务完成后）**

1. 1440 视口固定占用 ≤ 500px（当前 736px）。
2. 1280 视口缩略图区 ≥ 4 列（`getBoundingClientRect` 计列宽 ≥ 4×180）。
3. 1000 视口无横向滚动条，主导航为图标条，图库侧栏为抽屉。
4. 顶栏搜索回车 → 图库网格按关键词过滤（接口 `GET /api/library/assets?...` 带 `q`）。
5. 任务入口只有一个（侧栏状态条），点开是同一个抽屉。
6. `/compliance` ↔ 图库合规筛选双向可跳。
7. `pnpm vitest run` 全绿、`tsc -b` 0 error、`vite build` 成功。

---

## 7. 风险、权衡与开放问题

**风险**
- 与在跑卡冲突：`t_cef05116`（合规页）、`t_bf8f7e95`（图库删除/筛选横幅）、`t_9642d1d1`（弹窗，已 done）都改 `web/src`。**本计划必须在它们合并后再开工**，或只做不与它们重叠的文件（`App.tsx`、`lib/layoutPrefs.ts`、`lib/useMediaQuery.ts`、`components/DropOverlay.tsx`、`components/CollapsibleSection.tsx`、`pages/Settings.tsx`、`pages/TagTasks.tsx`）。开工前先 `git fetch gitea && git log --oneline gitea/main -5` 确认基线。
- 桌面壳与 web 形态共用 `web/`：拖拽导入在 web 形态拿不到本地路径，必须降级提示（Task 13 已写）。
- `matchMedia` 在 Tauri WebView2 与 Chrome 行为一致，但 Vitest（jsdom）需要 mock，Task 4 已给方案。
- 侧栏宽度拖拽与 `localStorage` 频繁写盘：改为 `mouseup` 时写一次，拖动中只改 `state`。

**权衡**
- 不重写页面结构（不做大重构），只做「折叠 + 断点 + 入口合并 + 分区」。原因是当前每条产品线都在跑，重构会与 5 张并行卡冲突。
- 搜索不做成"图库内联结果"，而是"顶栏提交 → 写图库筛选"。理由是搜索页已有语义搜索（文本塔 + 字面四通道），内联会重复实现一套；压一跳即可解决高频痛点。
- `/search` 页保留（语义搜索的工作台），只在导航分组里降级为「工作台」之一。

**开放问题（需产品裁决，实现者不要自己拍）**
1. Inspector 是否允许用户常驻关闭（现在收起只是空态行为），还是"选中必展开"？默认按后者实现（收起仅在未选中时）。
2. 导航是否要支持自定义排序/隐藏（如不需要，YAGNI 不做）。
3. 合规是否最终并入图库成为"视图之一"（真机实测用户抱怨"合规页看不到合格图"）：本计划只做互链，并入视图留给后续形态裁决。
4. 标签段是否需要"按使用频率/首字母"排序切换（当前按计数降序），需要一次真机数据验证后再定。
