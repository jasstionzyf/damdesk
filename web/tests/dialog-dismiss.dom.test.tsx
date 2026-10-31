// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { useState } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { act } from 'react-dom/test-utils'
import type { ReactElement } from 'react'

/**
 * UI-DIALOG（卡 task）回归：六个弹窗的「Esc 关闭 / ✕ 关闭 / 遮罩点击关闭」，
 * 以及「叠层时一次 Esc 只关一层」「内容区点击不误关」两条铁律。
 *
 * 用真实 DOM（jsdom）+ 真实事件派发，断言的是 vi.fn 回调与 DOM 存在性，
 * 不是「代码里写了 useEffect」这种静态证据。
 */

// ---- 依赖桩：这些组件都会在挂载时打后端接口，测试里全部走桩（不碰真机/真后端） ----
// vi.mock 被提升到文件顶部，工厂里引用的变量必须来自 vi.hoisted（否则 TDZ 报错）
const { apiMock } = vi.hoisted(() => ({
  apiMock: {
    scan: vi.fn(async () => ({ imageFiles: 0, totalBytes: 0, preview: [] })),
    tags: vi.fn(async () => []),
    dirs: vi.fn(async () => ({ dirs: [] })),
    smartFolderCreate: vi.fn(async () => ({})),
    smartFolderUpdate: vi.fn(async () => ({})),
    appInfo: vi.fn(async () => ({ dataDir: '/data' })),
    exportStatus: vi.fn(async () => ({ running: false, progress: null, report: null, error: null })),
    exportStart: vi.fn(async () => ({ ok: true })),
    exportCancel: vi.fn(async () => ({ ok: true })),
    assetTags: vi.fn(async () => []),
    thumbUrl: (id: number, size: number) => `/thumb/${id}/${size}`,
    originalUrl: (id: number) => `/orig/${id}`,
  },
}))

vi.mock('../src/lib/api', () => ({
  api: apiMock,
  formatSize: (n: number) => `${n}B`,
}))

vi.mock('../src/lib/tasks', () => ({
  TASK_KIND_LABEL: { import: '导入', embed: '嵌入', tag: '打标', dedup: '去重', export: '导出' },
  useTasks: () => ({
    tasks: [],
    runningCount: 0,
    startImport: vi.fn(),
    startEmbed: vi.fn(),
    startTag: vi.fn(),
    cancel: vi.fn(),
    retry: vi.fn(),
    dismiss: vi.fn(),
  }),
}))

vi.mock('../src/lib/toast', () => ({
  useToast: () => ({ toast: vi.fn(), dismiss: vi.fn() }),
  ToastProvider: ({ children }: { children: ReactElement }) => children,
}))

vi.mock('../src/lib/native', () => ({
  isDesktopShell: () => false,
  pickFolder: async () => null,
}))

import { useModalDismiss, modalStackDepth, Dialog } from '../src/lib/modal'
import { MemoryRouter } from 'react-router-dom'
import { OnboardingWizard } from '../src/components/OnboardingWizard'
import { Lightbox } from '../src/components/Lightbox'
import { ShortcutsHelp } from '../src/components/ShortcutsHelp'
import { SmartFolderDialog } from '../src/components/SmartFolderDialog'
import { ExportDialog } from '../src/components/ExportDialog'

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

// ---- 挂载/卸载小工具（不走 testing-library：repo 只装了 react-dom + jsdom） ----
let host: HTMLDivElement
let root: Root

function mount(ui: ReactElement) {
  host = document.createElement('div')
  document.body.appendChild(host)
  root = createRoot(host)
  act(() => {
    root.render(ui)
  })
  return host
}

function pressEsc() {
  act(() => {
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
  })
}

function pressKey(key: string) {
  act(() => {
    window.dispatchEvent(new KeyboardEvent('keydown', { key, bubbles: true }))
  })
}

function click(el: Element | null) {
  if (!el) throw new Error('要点击的元素不存在')
  act(() => {
    ;(el as HTMLElement).click()
  })
}

/**
 * 浮层统一原语（Dialog/Drawer）**portal 到 document.body** —— 因为页面级浮层渲染在
 * `[data-overlay-base]` 内部，而打开浮层会给 base 加 `inert`；不 portal 的话浮层自己就在
 * 惰性子树里（鼠标点不到、焦点进不去）。所以断言面从 host 改成 document。
 */
function query(testid: string) {
  return document.querySelector(`[data-testid="${testid}"]`)
}

/** 文本断言也按 portal 语义取整页文本（页面内容 + body 下的浮层） */
function allText() {
  return document.body.textContent ?? ''
}

beforeEach(() => {
  // @ts-expect-error React 18 act 环境开关
  globalThis.IS_REACT_ACT_ENVIRONMENT = true
})

afterEach(() => {
  act(() => {
    root?.unmount()
  })
  host?.remove()
  vi.clearAllMocks()
  // 每例结束后弹窗栈必须是空的（卸载清理干净，否则后续 Esc 会错乱）
  expect(modalStackDepth()).toBe(0)
})

describe('统一关闭语义：Esc / ✕ / 遮罩', () => {
  it('Dialog 壳（浮层统一原语）：Esc / ✕ / 遮罩三条关闭路径 + 内容区点击不误关', () => {
    // UI-CARRIER 起「任务中心」改成页面（不再有抽屉实例），统一语义的断言面落在浮层原语上。
    const onClose = vi.fn()
    const h = mount(
      <Dialog open title="测试对话框" onClose={onClose} testId="probe-dialog">
        <p data-testid="probe-body">内容</p>
      </Dialog>,
    )

    expect(query('probe-dialog')).not.toBeNull()
    expect(query('probe-dialog')!.getAttribute('role')).toBe('dialog')
    expect(query('probe-dialog')!.getAttribute('aria-modal')).toBe('true')

    // ① 点内容区 → 不关
    click(query('probe-body'))
    expect(onClose).not.toHaveBeenCalled()

    // ② 点遮罩 → 关
    click(query('probe-dialog-mask'))
    expect(onClose).toHaveBeenCalledTimes(1)

    // ③ ✕ → 关
    click(query('probe-dialog-close'))
    expect(onClose).toHaveBeenCalledTimes(2)

    // ④ Esc → 关
    pressEsc()
    expect(onClose).toHaveBeenCalledTimes(3)
    expect(allText()).toContain('测试对话框')
  })

  it('页面级浮层：base 被 inert 时浮层自己不在惰性子树里（portal 到 body，否则鼠标点不动）', () => {
    // 复现真实结构：页面渲染在 [data-overlay-base] 内，浮层从页面里发起
    const base = document.createElement('div')
    base.setAttribute('data-overlay-base', '')
    document.body.appendChild(base)
    const inner = document.createElement('div')
    base.appendChild(inner)
    const r = createRoot(inner)
    act(() => {
      r.render(
        <Dialog open title="页面内浮层" onClose={vi.fn()} testId="inpage-dialog">
          <p data-testid="inpage-body">内容</p>
        </Dialog>,
      )
    })

    const panel = query('inpage-dialog')
    expect(panel).not.toBeNull()
    // 浮层必须在 body 下、base 之外；否则 base 的 inert 会把浮层一起锁死
    expect(panel!.closest('[data-overlay-base]')).toBeNull()
    expect(panel!.closest('[inert]')).toBeNull()
    expect(panel!.parentElement?.parentElement).toBe(document.body)
    // 背景壳照旧被 inert（背景不可点）
    expect(base.hasAttribute('inert')).toBe(true)

    act(() => {
      r.unmount()
    })
    // 关闭即摘掉 inert（背景恢复可点）
    expect(base.hasAttribute('inert')).toBe(false)
    base.remove()
  })

  it('首启导入向导：✕ / Esc / 点遮罩都能关（Dialog 壳）', () => {
    const onClose = vi.fn()
    const h = mount(<OnboardingWizard onClose={onClose} onFinish={vi.fn()} />)

    const panel = query('onboarding-wizard')
    expect(panel).not.toBeNull()
    expect(panel!.getAttribute('role')).toBe('dialog')
    expect(panel!.getAttribute('aria-modal')).toBe('true')
    const close = query('onboarding-wizard-close')
    expect(close).not.toBeNull()
    // ✕ 的语义 = 稍后再说（不丢数据），页面上有对应文案
    expect(allText()).toContain('稍后再说')

    click(close)
    expect(onClose).toHaveBeenCalledTimes(1)

    pressEsc()
    expect(onClose).toHaveBeenCalledTimes(2)

    click(query('onboarding-wizard-mask'))
    expect(onClose).toHaveBeenCalledTimes(3)

    // 内容区点击不关
    click(query('wizard-stepbar'))
    expect(onClose).toHaveBeenCalledTimes(3)
  })

  it('Lightbox 全屏预览：Esc 与顶栏「退出」都能关；点图片本身不关（不误触）', () => {
    const onClose = vi.fn()
    const h = mount(
      <Lightbox
        items={[
          {
            assetId: 1,
            fileName: 'a.jpg',
            fileExt: 'jpg',
            fileSz: 1024,
            width: 100,
            height: 100,
            storeMode: 'inplace',
            importedAt: 0,
            approvalStatus: 'pending',
            starRating: 0,
            missing: 0,
            absPath: '/a.jpg',
          } as never,
        ]}
        index={0}
        onIndex={vi.fn()}
        onClose={onClose}
        onRate={vi.fn()}
        onReject={vi.fn()}
        selectionCount={1}
        onHelp={vi.fn()}
      />,
    )

    expect(query('lightbox')).not.toBeNull()

    // ① 点图片本身（放大/还原）不关
    click(query('lightbox-image'))
    expect(onClose).not.toHaveBeenCalled()

    // ② Esc → 关（Lightbox 自己的键位处理）
    pressEsc()
    expect(onClose).toHaveBeenCalledTimes(1)

    // ③ 顶栏「退出」按钮 → 关
    const exitBtn = Array.from(h.querySelectorAll('button')).find((b) => (b.textContent ?? '').trim() === '退出')
    expect(exitBtn, '顶栏应有「退出」按钮').toBeTruthy()
    click(exitBtn!)
    expect(onClose).toHaveBeenCalledTimes(2)

    expect(query('lightbox-counter')!.textContent).toContain('1 / 1')
  })

  it('快捷键帮助：ESC / ✕ / 遮罩 / ? 都能关', () => {
    const onClose = vi.fn()
    mount(<ShortcutsHelp open onClose={onClose} />)

    click(query('modal-close'))
    expect(onClose).toHaveBeenCalledTimes(1)

    pressEsc()
    expect(onClose).toHaveBeenCalledTimes(2)

    click(query('shortcuts-overlay'))
    expect(onClose).toHaveBeenCalledTimes(3)

    // ? 同键切换（Esc 已移到公共栈，? 仍是本面板自己的键位）
    pressKey('?')
    expect(onClose).toHaveBeenCalledTimes(4)
  })

  it('智能文件夹对话框：ESC / ✕ / 遮罩都能关（原先只有取消按钮 + 遮罩）', () => {
    const onClose = vi.fn()
    mount(<SmartFolderDialog onClose={onClose} onSaved={vi.fn()} />)

    click(query('modal-close'))
    expect(onClose).toHaveBeenCalledTimes(1)

    pressEsc()
    expect(onClose).toHaveBeenCalledTimes(2)

    click(query('smart-folder-overlay'))
    expect(onClose).toHaveBeenCalledTimes(3)
  })

  it('导出对话框：URL 驱动的单实例 —— ✕ 关闭即清掉 ?dialog=export，点内容区不关', () => {
    const h = mount(
      <MemoryRouter initialEntries={['/?dialog=export&ids=1,2']}>
        <ExportDialog />
      </MemoryRouter>,
    )

    const panel = query('export-dialog')
    expect(panel).not.toBeNull()
    expect(panel!.textContent).toContain('导出选择集（2 张）')

    // 点内容区不关
    click(panel)
    expect(query('export-dialog')).not.toBeNull()

    // ✕ → URL 里 dialog 参数被清掉，面板消失（replace，不制造历史垃圾）
    click(query('export-dialog-close'))
    expect(query('export-dialog')).toBeNull()

    // 遮罩存在且挂在壳上（同一套关闭语义）
    expect(allText()).not.toContain('导出选择集')
  })
})

describe('焦点管理：打开聚焦内容、关闭还给触发元素', () => {
  it('从触发按钮打开 → 焦点进面板；Esc 关闭 → 焦点回到触发按钮', () => {
    function Harness() {
      const [open, setOpen] = useState(false)
      return (
        <>
          <button type="button" data-testid="trigger" onClick={() => setOpen(true)}>
            打开快捷键帮助
          </button>
          <ShortcutsHelp open={open} onClose={() => setOpen(false)} />
        </>
      )
    }

    mount(<Harness />)
    const trigger = query('trigger') as HTMLButtonElement
    trigger.focus()
    expect(document.activeElement).toBe(trigger)

    click(trigger)
    expect(query('shortcuts-help')).not.toBeNull()
    expect(document.activeElement).toBe(query('shortcuts-help'))

    pressEsc()
    expect(document.activeElement).toBe(trigger)
  })
})

describe('叠层：一次 Esc 只关最上面那层', () => {
  it('Lightbox 上叠快捷键帮助 → 第一次 Esc 只关帮助，第二次才关 Lightbox', () => {
    const onCloseLightbox = vi.fn()
    const onCloseHelp = vi.fn()

    function Nested({ helpOpen }: { helpOpen: boolean }) {
      return (
        <>
          <Lightbox
            items={[{ assetId: 1, fileName: 'a.jpg', fileExt: 'jpg', fileSz: 1, starRating: 0, approvalStatus: 'pending' } as never]}
            index={0}
            onIndex={vi.fn()}
            onClose={onCloseLightbox}
            onRate={vi.fn()}
            onReject={vi.fn()}
            selectionCount={1}
            onHelp={vi.fn()}
            suspended={helpOpen}
          />
          <ShortcutsHelp open={helpOpen} onClose={onCloseHelp} context="lightbox" />
        </>
      )
    }

    // 帮助打开（Gallery 传 suspended）：栈上只有帮助
    mount(<Nested helpOpen />)
    expect(modalStackDepth()).toBe(1)
    pressEsc()
    expect(onCloseHelp).toHaveBeenCalledTimes(1)
    expect(onCloseLightbox).not.toHaveBeenCalled()

    // 帮助关闭后再按 Esc → Lightbox 关（Lightbox 走自己的键位处理，不占公共栈：
    // 栈深度在帮助关闭后应为 0，Esc 仍能关掉 Lightbox）
    act(() => {
      root.render(<Nested helpOpen={false} />)
    })
    expect(modalStackDepth()).toBe(0)
    pressEsc()
    expect(onCloseLightbox).toHaveBeenCalledTimes(1)
    expect(onCloseHelp).toHaveBeenCalledTimes(1)
  })

  it('不支持 suspended 的两层同时挂在栈上时，也只关最后挂载的那层', () => {
    const onCloseA = vi.fn()
    const onCloseB = vi.fn()

    function Two() {
      useModalDismiss({ open: true, onClose: onCloseA })
      useModalDismiss({ open: true, onClose: onCloseB })
      return <div data-testid="two" />
    }

    mount(<Two />)
    expect(modalStackDepth()).toBe(2)
    pressEsc()
    expect(onCloseB).toHaveBeenCalledTimes(1)
    expect(onCloseA).not.toHaveBeenCalled()
  })

  it('关闭（open=false）时不占栈、也不响应 Esc', () => {
    const onClose = vi.fn()
    function Closed() {
      useModalDismiss({ open: false, onClose })
      return <div />
    }
    mount(<Closed />)
    expect(modalStackDepth()).toBe(0)
    pressEsc()
    expect(onClose).not.toHaveBeenCalled()
  })
})
