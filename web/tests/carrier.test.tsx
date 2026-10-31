import { describe, expect, it, vi } from 'vitest'
import { renderToStaticMarkup } from 'react-dom/server'
import { MemoryRouter, Route, Routes } from 'react-router-dom'

import { Dialog, Drawer, OVERLAY_BASE_ATTR } from '../src/lib/modal'
import { ExportDialog, parseIdsParam } from '../src/components/ExportDialog'
import { Trash } from '../src/pages/Trash'
import { Tasks } from '../src/pages/Tasks'
import { TaskCenterProvider } from '../src/lib/tasks'
import { ToastProvider } from '../src/lib/toast'
import { ConfirmProvider } from '../src/lib/confirm'

// i18n-C（卡 task）：本文件断言的是**中文界面原文**，而迁移后文案走 t()、
// jsdom 的 navigator.language 默认 en-US（会渲染英文）。这里把语言钉成 zh-Hans ——
// t() 在中文下缺译回退 key（= 中文原文），断言逐字不变、强度不降，只是显式声明
// 「本文件验中文文案」。vi.hoisted 早于所有 import 执行，故早于 i18n 模块初始化。
vi.hoisted(() => {
  // jsdom 文件有真实 localStorage；node 环境的纯 SSR 渲染测试没有它 ——
  // 给一个最小桩，目的只有一个：让 i18n 初始化时 resolveInitialLocale() 读到 zh-Hans。
  try {
    if (typeof localStorage !== 'undefined' && localStorage) {
      localStorage.setItem('dam.locale', 'zh-Hans')
      return
    }
  } catch {
    /* 落桩 */
  }
  vi.stubGlobal('localStorage', {
    getItem: (k: string) => (k === 'dam.locale' ? 'zh-Hans' : null),
    setItem: () => {},
    removeItem: () => {},
    clear: () => {},
    key: () => null,
    length: 0,
  })
})

/**
 * UI-CARRIER-1（卡 task）载体结构单测：
 *  - 深链直达：`/trash`、`/tasks?task=`、`?dialog=export&ids=` 各自渲染出目标载体；
 *  - 载体外壳的四条硬要求：aria-labelledby 必须指向**可见标题**、kind 标记正确、单一滚动容器、
 *    背景 inert 的目标容器存在（真实行为见 scripts/ui_carrier_probe.mjs 的 CDP 证据）；
 *  - 「不伪造」：回收站彻底删除的能力缺口必须在 UI 上写明（不留假按钮）。
 *
 * 纯 SSR 渲染（不跑 effect / 不发请求），与仓库既有 DOM 存证测试同一套路。
 */
function ssr(node: React.ReactElement): string {
  // KI-11 起回收站要弹「彻底删除」确认框 → 页面依赖 ConfirmProvider
  return renderToStaticMarkup(
    <ToastProvider>
      <ConfirmProvider>{node}</ConfirmProvider>
    </ToastProvider>,
  )
}

/** 从 markup 里取 aria-labelledby 指向的 id 对应元素文本 —— 证明标题「可见」而非只活在属性里 */
function labelledText(markup: string): string | null {
  const m = /aria-labelledby="([^"]+)"/.exec(markup)
  if (!m) return null
  const id = m[1]
  const el = new RegExp(`<h2 id="${id}"[^>]*>(.*?)</h2>`, 's').exec(markup)
  return el ? el[1].replace(/<[^>]+>/g, '') : null
}

describe('深链直达：状态页（回收站 / 任务）', () => {
  it('/trash 直接渲染回收站页面（页面载体，不是弹窗）', () => {
    const html = ssr(
      <MemoryRouter initialEntries={['/trash']}>
        <Routes>
          <Route path="/trash" element={<Trash />} />
        </Routes>
      </MemoryRouter>,
    )
    console.log('[DOM] /trash =', html.slice(0, 260), '…')
    expect(html).toContain('data-testid="page-trash"')
    expect(html).toContain('回收站（0）')
    expect(html).toContain('data-testid="trash-restore-selected"')
    // 页面载体：没有 role="dialog"（不许把回收站做成 modal）
    expect(html).not.toContain('role="dialog"')
  })

  it('/trash：彻底删除写明「先确认、不可撤销、磁盘边界」（不伪造立即删除）', () => {
    const html = ssr(
      <MemoryRouter initialEntries={['/trash']}>
        <Routes>
          <Route path="/trash" element={<Trash />} />
        </Routes>
      </MemoryRouter>,
    )
    expect(html).toContain('data-testid="trash-purge-selected"')
    // 未选任何图 → 按钮灰置（选中 ≠ 执行）
    expect(html).toContain('disabled')
    expect(html).toContain('data-testid="trash-purge-note"')
    // 能力位加载前不灰置入口，但说明必须讲清「先弹确认 + 不可撤销 + copyin/reference 差异」
    expect(html).toContain('先弹确认框')
    expect(html).toContain('copyin')
    expect(html).toContain('reference')
  })

  it('/tasks?task=42 渲染任务页并定位该行（可回看 = 页面判据）', () => {
    const html = ssr(
      <MemoryRouter initialEntries={['/tasks?task=42']}>
        <TaskCenterProvider>
          <Routes>
            <Route path="/tasks" element={<Tasks />} />
          </Routes>
        </TaskCenterProvider>
      </MemoryRouter>,
    )
    console.log('[DOM] /tasks =', html.slice(0, 260), '…')
    expect(html).toContain('data-testid="page-tasks"')
    expect(html).toContain('data-testid="task-focus-hint"')
    // KI35-P3：/tasks 用户面改名「处理记录」，定位提示跟着改
    expect(html).toContain('已定位到记录 #42')
    // DM-B2（KI-37）：导出/导入不再是页面上的独立分区，而是同一张 job 列表里的行（L2 展开）
    expect(html).toContain('data-testid="job-list-section"')
    expect(html).not.toContain('role="dialog"')
  })
})

describe('深链直达：Dialog / Drawer 载体', () => {
  it('?dialog=export&ids=3,9 直接打开导出配置 Dialog，标题带真实张数', () => {
    const html = ssr(
      <MemoryRouter initialEntries={['/trash?dialog=export&ids=3,9']}>
        <ExportDialog />
      </MemoryRouter>,
    )
    console.log('[DOM] export dialog =', html.slice(0, 320), '…')
    expect(html).toContain('data-testid="export-dialog"')
    expect(html).toContain('data-overlay-kind="dialog"')
    expect(html).toContain('导出选择集（2 张）')
    // aria-labelledby 指的必须是**看得见**的标题
    expect(labelledText(html)).toBe('导出选择集（2 张）')
    // ≤6 字段一次性表单：只允许一层滚动容器（不许二级滚动）
    const scrollLayers = (html.match(/overflow-y-auto/g) ?? []).length
    expect(scrollLayers).toBe(1)
  })

  it('ids 参数脏值不炸页面（非法项丢弃）', () => {
    expect(parseIdsParam('3,9')).toEqual([3, 9])
    expect(parseIdsParam('3,abc,,9 ,-2,0')).toEqual([3, 9])
    expect(parseIdsParam(null)).toEqual([])
    const html = ssr(
      <MemoryRouter initialEntries={['/?dialog=export&ids=abc']}>
        <ExportDialog />
      </MemoryRouter>,
    )
    expect(html).toContain('导出选择集（0 张）')
  })

  it('Drawer 外壳：kind=drawer + aria-labelledby 指可见标题 + 单滚动容器', () => {
    const html = renderToStaticMarkup(
      <Drawer open title="示例抽屉（12）" subtitle="外壳契约用例，与任何具体业务无关" onClose={() => {}} testId="carrier-drawer">
        <p>body</p>
      </Drawer>,
    )
    console.log('[DOM] drawer =', html.slice(0, 260), '…')
    expect(html).toContain('data-overlay-kind="drawer"')
    expect(html).toContain('aria-modal="true"')
    expect(labelledText(html)).toBe('示例抽屉（12）')
    expect((html.match(/overflow-y-auto/g) ?? []).length).toBe(1)
    // 三通道出口：✕ 按钮 + Esc（keydown 处理在 shell 里）+ 遮罩
    expect(html).toContain('aria-label="关闭"')
    expect(html).toContain('data-overlay-mask="drawer"')
  })

  it('Dialog 外壳：关闭状态不渲染任何 DOM（无幽灵遮罩）', () => {
    const html = renderToStaticMarkup(
      <Dialog open={false} title="不该出现" onClose={() => {}}>
        <p>x</p>
      </Dialog>,
    )
    expect(html).toBe('')
  })

  it('背景 inert 的目标容器由常量统一（App 壳标记，浮层不许渲染在它内部）', () => {
    expect(OVERLAY_BASE_ATTR).toBe('data-overlay-base')
  })
})

describe('载体自检：反模式', () => {
  it('长列表不进 Dialog：回收站/任务页均为页面（无 role=dialog）', () => {
    const trash = ssr(
      <MemoryRouter initialEntries={['/trash']}>
        <Routes>
          <Route path="/trash" element={<Trash />} />
        </Routes>
      </MemoryRouter>,
    )
    const tasks = ssr(
      <MemoryRouter initialEntries={['/tasks']}>
        <TaskCenterProvider>
          <Routes>
            <Route path="/tasks" element={<Tasks />} />
          </Routes>
        </TaskCenterProvider>
      </MemoryRouter>,
    )
    expect(trash).not.toContain('role="dialog"')
    expect(tasks).not.toContain('role="dialog"')
  })
})
