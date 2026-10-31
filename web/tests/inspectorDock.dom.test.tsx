// @vitest-environment jsdom
// @vitest-environment-options {"url": "http://localhost:3000/gallery"}
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { BrowserRouter } from 'react-router-dom'
import { ToastProvider } from '../src/lib/toast'
import { ConfirmProvider } from '../src/lib/confirm'
import { DockedPanel } from '../src/lib/modal'

/**
 * DOCKED（卡 task）：检查器改右侧**非模态**抽屉。
 *
 * 事实基线（方案见 .agent/plans/2026-10-19_155819-inspector-docked-panel.md）：
 *  - 检查器不再常驻第三栏（1280 视口主区从 512 → 832，止「中文竖排」）；
 *  - 选中即自动滑出，**不遮罩不锁背景**（能边看图边看详情）；
 *  - 用户手动关掉后**本会话不再自动弹**（sessionStorage 记忆），再手动打开则恢复；
 *  - 一个能力一个入口：Lightbox 的 I 信息面板已删，详情只在抽屉里看；
 *  - Esc 优先级：Lightbox > 对话框 > 检查器抽屉 > 树抽屉 > 清空筛选。
 *
 * jsdom 里 VirtuosoGrid 不渲染卡片（无卡片可点），因此「选中」一律用 URL 的 `?ids=`
 * 深链进入 —— 这也是真实刷新/分享路径（选择集写-through 进 URL）。
 */

function memStorage(): Storage {
  const store = new Map<string, string>()
  return {
    getItem: (k: string) => store.get(k) ?? null,
    setItem: (k: string, v: string) => void store.set(k, String(v)),
    removeItem: (k: string) => void store.delete(k),
    clear: () => store.clear(),
    key: (i: number) => [...store.keys()][i] ?? null,
    get length() {
      return store.size
    },
  } satisfies Storage
}

// jsdom 的 localStorage/sessionStorage 在 vitest 里拿不到（本仓既有测试同样手动打桩）：
// 给两份独立内存实现 —— 检查器的「本会话不再自动弹」记忆就落在 sessionStorage。
Object.defineProperty(globalThis, 'localStorage', { value: memStorage(), configurable: true, writable: true })
Object.defineProperty(globalThis, 'sessionStorage', { value: memStorage(), configurable: true, writable: true })

const ASSETS = [
  { assetId: 11, fileName: 'snow-1.jpg', titleCh: '雪山日出' },
  { assetId: 12, fileName: 'snow-2.jpg', titleCh: '雪山云海' },
  { assetId: 13, fileName: 'beach.jpg', titleCh: '海滩' },
]

function assetOf(a: (typeof ASSETS)[number]) {
  return {
    assetId: a.assetId,
    mediaType: 'image',
    absPath: `/library/${a.fileName}`,
    storeMode: 'index',
    missing: 0,
    fileName: a.fileName,
    fileExt: 'jpg',
    fileSz: 2048,
    mtime: 0,
    fileHash: null,
    titleCh: a.titleCh,
    titleEn: null,
    description: null,
    source: 'index',
    parentAssetId: null,
    searchable: 1,
    deleted: 0,
    approvalStatus: 'pending',
    starRating: 0,
    colorLabel: null,
    cloudAssetId: null,
    importedAt: 0,
    embStatus: 'done',
    embModel: 'stub',
    dedupStatus: 'none',
    dedupModel: null,
    thumbPath: null,
    width: 1200,
    height: 800,
    pHash: null,
    takenAt: null,
    cameraMake: null,
    cameraModel: null,
    lensModel: null,
    iso: null,
    focalLenMm: null,
    aperture: null,
    shutterText: null,
    orientation: null,
    aspectRatio: null,
    aspectClass: null,
    gpsLat: null,
    gpsLon: null,
    gpsSource: null,
    exif: null,
  }
}

const apiStub = vi.hoisted(() => ({
  stats: vi.fn(),
  dirs: vi.fn(),
  tags: vi.fn(),
  complianceStatus: vi.fn(),
  importStatus: vi.fn(),
  assets: vi.fn(),
  capabilities: vi.fn(),
  assetTags: vi.fn(),
  exportStatus: vi.fn(),
  appInfo: vi.fn(),
  cameras: vi.fn(),
  complianceVerdicts: vi.fn(),
  complianceReview: vi.fn(),
  complianceReviewCreate: vi.fn(),
  complianceReviewRevoke: vi.fn(),
  ocrAssetText: vi.fn(),
  smartFolder: vi.fn(),
  smartFolderDelete: vi.fn(),
  thumbUrl: (id: number, size = 256) => `/api/media/thumb/${id}?size=${size}`,
  originalUrl: (id: number) => `/api/media/original/${id}`,
}))

vi.mock('../src/lib/api', () => ({
  api: apiStub,
  formatSize: (n: number) => `${n} B`,
  ApiError: class ApiError extends Error {
    code?: string
    status = 500
    constructor(msg: string, status = 500, code?: string) {
      super(msg)
      this.code = code
      this.status = status
    }
  },
}))

apiStub.stats.mockImplementation(async () => ({
  assets: ASSETS.length,
  missing: 0,
  tagged: 0,
  totalBytes: 0,
  byMediaType: [{ mediaType: 'image', count: ASSETS.length }],
  missingCheckedAt: null,
  missingStale: false,
  missingUnreadable: 0,
  broken: 0,
  missingStaleSecs: 86400,
}))
apiStub.dirs.mockImplementation(async () => ({ dirs: [], roots: [] }))
apiStub.tags.mockImplementation(async () => [])
apiStub.complianceStatus.mockImplementation(async () => ({
  states: { pass: 0, fail: 0, unknown: 0, pending: 0, exempt: 0 },
  pack: { id: 'stub' },
}))
apiStub.importStatus.mockImplementation(async () => ({ running: false, progress: null }))
apiStub.assets.mockImplementation(async () => ({ total: ASSETS.length, items: ASSETS.map(assetOf) }))
apiStub.capabilities.mockImplementation(async () => ({ features: {} }))
apiStub.assetTags.mockImplementation(async () => [])
apiStub.exportStatus.mockImplementation(async () => ({ running: false, progress: null, report: null, error: null }))
apiStub.appInfo.mockImplementation(async () => ({ dataDir: '/data' }))
apiStub.cameras.mockImplementation(async () => ({ cameras: [] }))
// 检查器里的合规块（CompliancePanel）首屏就会拉一次判定明细
apiStub.complianceVerdicts.mockImplementation(async (assetId: number) => ({
  packId: 'stub',
  assetId,
  state: 'unknown',
  stateLabel: '未判定',
  machineState: 'unknown',
  machineStateLabel: '未判定',
  machineConfidence: null,
  assetHumanReview: null,
  items: [],
}))
// 检查器的 OCR 块（默认折叠但首屏就取一次）与合规改判历史
apiStub.ocrAssetText.mockImplementation(async (assetId: number) => ({ assetId, status: 'none', text: null, blocks: [] }))
apiStub.complianceReview.mockImplementation(async () => ({ items: [], total: 0, nextCursor: null }))

// 1024 视口 = 「<1280」档：库面板走抽屉（才能测「Esc 先关检查器、不吃掉树抽屉」）
Object.defineProperty(window, 'innerWidth', { value: 1024, writable: true, configurable: true })
Object.defineProperty(window, 'innerHeight', { value: 768, writable: true, configurable: true })

Object.defineProperty(window, 'matchMedia', {
  writable: true,
  configurable: true,
  value: (q: string) => ({
    matches: false,
    media: q,
    onchange: null,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    dispatchEvent: () => false,
  }),
})
;(globalThis as unknown as { ResizeObserver: unknown }).ResizeObserver = class {
  observe() {}
  unobserve() {}
  disconnect() {}
}

const { Gallery } = await import('../src/pages/Gallery')

function renderGallery(path = '/gallery') {
  window.history.replaceState(null, '', path)
  return render(
    <BrowserRouter>
      <ToastProvider>
        <ConfirmProvider>
          <Gallery onImport={() => {}} />
        </ConfirmProvider>
      </ToastProvider>
    </BrowserRouter>,
  )
}

const drawerParam = () => new URLSearchParams(window.location.search).get('drawer')
const dock = () => screen.queryByTestId('inspector-dock')

async function settle() {
  await waitFor(() => expect(document.body.textContent).toContain(`已加载 ${ASSETS.length}`))
}

/** 选中即自动滑出依赖一个 effect → 用 waitFor 等它落地（不要 sleep 猜时间） */
async function waitDockOpen() {
  await waitFor(() => expect(dock()).not.toBeNull())
}

describe('DOCKED 检查器抽屉（卡 task）', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    sessionStorage.clear()
    window.history.replaceState(null, '', '/gallery')
  })
  afterEach(() => cleanup())

  it('选中即自动滑出：非模态（无遮罩 / 无 aria-modal）、宽度不吃主区、URL 记 ?drawer=inspector', async () => {
    renderGallery('/gallery?ids=11')
    await settle()
    await waitDockOpen()

    const panel = dock()!
    expect(panel.getAttribute('role')).toBe('complementary')
    expect(panel.getAttribute('aria-modal')).toBeNull()
    // 非模态 = 没有遮罩层（既有的模态壳都会渲染 bg-black/40；抽屉一个都不许有）
    expect(document.querySelectorAll('[class*="bg-black/"]').length).toBe(0)
    // 面板宽度固定 360（<640 才全宽），不随内容/主区变化
    expect(panel.className).toContain('sm:w-[360px]')
    expect(panel.className).not.toContain('w-72')
    // 标题来自 DockedPanel header（不是 Inspector 自己的「检查器」小标题）
    expect(screen.getByTestId('inspector-dock-title').textContent).toBe('检查器')
    // URL 是开关的真源：刷新/深链/back 都能恢复
    expect(drawerParam()).toBe('inspector')
    // 主区宽度不再给检查器让位：库面板此时按「<1280」走抽屉（tree-toggle 把手在）
    expect(screen.getByTestId('tree-toggle')).toBeTruthy()
  })

  it('点 ✕ 关闭 → 本会话再选另一张不再自动弹', async () => {
    renderGallery('/gallery?ids=11')
    await settle()
    await waitDockOpen()

    fireEvent.click(screen.getByTestId('inspector-dock-close'))
    await waitFor(() => expect(dock()).toBeNull())
    expect(drawerParam()).toBeNull()
    // 关闭只记「本会话不再自动弹」，选择集不受影响（ids 还在 URL，页面 write-through 未被顶掉）
    expect(new URLSearchParams(window.location.search).get('ids')).toBe('11')
    expect(sessionStorage.getItem('dam.inspector.autoClose:gallery')).toBe('1')

    // 卸载后重新进入（= 用户再点另一张）：有选中但不再自动弹
    cleanup()
    renderGallery('/gallery?ids=12')
    await settle()
    expect(dock()).toBeNull()
    expect(drawerParam()).toBeNull()
  })

  it('手动点工具条「详情」→ 重新打开并清掉抑制标记（此后恢复自动弹）', async () => {
    renderGallery('/gallery?ids=11')
    await settle()
    await waitDockOpen()
    fireEvent.click(screen.getByTestId('inspector-dock-close'))
    await waitFor(() => expect(dock()).toBeNull())
    expect(sessionStorage.getItem('dam.inspector.autoClose:gallery')).toBe('1')

    fireEvent.click(screen.getByTestId('inspector-toggle'))
    await waitDockOpen()
    expect(drawerParam()).toBe('inspector')
    // 手动打开 = 用户明确要它 → 抑制标记清掉
    expect(sessionStorage.getItem('dam.inspector.autoClose:gallery')).toBeNull()

    // 恢复自动弹：重新进入另一张也自动滑出
    cleanup()
    renderGallery('/gallery?ids=13')
    await settle()
    await waitDockOpen()
  })

  it('无选中时「详情」按钮灰置（没有可展示的资产就不弹空面板）', async () => {
    renderGallery('/gallery')
    await settle()
    const btn = screen.getByTestId('inspector-toggle') as HTMLButtonElement
    expect(btn.disabled).toBe(true)
    expect(dock()).toBeNull()
    // 未选中不自动弹（URL 也不留 drawer）
    expect(drawerParam()).toBeNull()
  })

  it('Esc 先关检查器抽屉，不吃掉库面板抽屉（一次 Esc 只关一层）', async () => {
    renderGallery('/gallery?ids=11')
    await settle()
    await waitDockOpen()

    // 打开库面板抽屉（<1280 档）
    fireEvent.click(screen.getByTestId('tree-toggle'))
    await waitFor(() => expect(screen.getByTestId('tree-drawer')).toBeTruthy())

    fireEvent.keyDown(window, { key: 'Escape' })
    await waitFor(() => expect(dock()).toBeNull())
    // 树抽屉还在（它要等下一发 Esc）
    expect(screen.getByTestId('tree-drawer')).toBeTruthy()
    // 选择集也没被「清空筛选」抢走
    expect(new URLSearchParams(window.location.search).get('ids')).toBe('11')

    // 第二发 Esc 才轮到树抽屉
    fireEvent.keyDown(window, { key: 'Escape' })
    await waitFor(() => expect(screen.queryByTestId('tree-drawer')).toBeNull())
  })

  it('焦点在输入框里按 Esc：先归输入框，不关抽屉（避免编辑标签时误关）', async () => {
    renderGallery('/gallery?ids=11')
    await settle()
    await waitDockOpen()

    const input = screen.getByTestId('tag-input')
    input.focus()
    fireEvent.keyDown(input, { key: 'Escape' })
    // 抽屉仍在（Esc 被输入框消费）
    expect(dock()).not.toBeNull()
    expect(drawerParam()).toBe('inspector')
  })

  it('抽屉开着时再点「详情」= 关闭（开关语义，且同样记「本会话不再自动弹」）', async () => {
    renderGallery('/gallery?ids=11')
    await settle()
    await waitDockOpen()

    fireEvent.click(screen.getByTestId('inspector-toggle'))
    await waitFor(() => expect(dock()).toBeNull())
    expect(sessionStorage.getItem('dam.inspector.autoClose:gallery')).toBe('1')
    expect(drawerParam()).toBeNull()
  })

  it('深链 ?drawer=inspector 直接进入即打开（刷新 / 分享同一状态）', async () => {
    renderGallery('/gallery?ids=11&drawer=inspector')
    await settle()
    await waitDockOpen()
    expect(dock()).not.toBeNull()
  })

  it('多选（?ids=11,12）→ 批量编辑面板：标题/张数走抽屉 header，工具条按钮同步为开', async () => {
    renderGallery('/gallery?ids=11,12')
    await settle()
    await waitDockOpen()
    expect(screen.getByTestId('inspector-dock-title').textContent).toBe('批量编辑')
    expect(screen.getByTestId('inspector-dock-subtitle').textContent).toContain('已选 2 张')
    // 选中即弹（批量同样自动开）
    expect(drawerParam()).toBe('inspector')
  })
})

describe('DockedPanel 壳（非模态抽屉原语）', () => {
  afterEach(() => cleanup())

  it('open=false 渲染 null；open=true 出 header/body，宽度类可覆盖', () => {
    const { rerender } = render(
      <DockedPanel open={false} onClose={() => {}} title="检查器" testId="t-dock">
        body
      </DockedPanel>,
    )
    expect(screen.queryByTestId('t-dock')).toBeNull()

    rerender(
      <DockedPanel open onClose={() => {}} title="检查器" subtitle="副标题" testId="t-dock" widthClass="w-[420px]">
        body
      </DockedPanel>,
    )
    expect(screen.getByTestId('t-dock').className).toContain('w-[420px]')
    expect(screen.getByTestId('t-dock-title').textContent).toBe('检查器')
    expect(screen.getByTestId('t-dock-subtitle').textContent).toBe('副标题')
    expect(screen.getByTestId('t-dock-body').textContent).toBe('body')
  })

  it('footer 只在给了才渲染；✕ 与 Esc 都调 onClose', () => {
    const onClose = vi.fn()
    const { rerender } = render(
      <DockedPanel open onClose={onClose} title="检查器" testId="t-dock">
        body
      </DockedPanel>,
    )
    expect(screen.queryByTestId('t-dock-footer')).toBeNull()
    fireEvent.click(screen.getByTestId('t-dock-close'))
    expect(onClose).toHaveBeenCalledTimes(1)

    rerender(
      <DockedPanel open onClose={onClose} title="检查器" testId="t-dock" footer={<span>底栏</span>}>
        body
      </DockedPanel>,
    )
    fireEvent.keyDown(window, { key: 'Escape' })
    expect(onClose).toHaveBeenCalledTimes(2)
    expect(document.body.textContent).toContain('底栏')
  })

  it('suspended：上层浮层打开时挂起 Esc（一次 Esc 只关一层）', () => {
    const onClose = vi.fn()
    render(
      <DockedPanel open onClose={onClose} title="检查器" testId="t-dock" suspended>
        body
      </DockedPanel>,
    )
    fireEvent.keyDown(window, { key: 'Escape' })
    expect(onClose).not.toHaveBeenCalled()
  })

  it('Esc 落在输入控件上：先归控件（取消输入/清空），不关抽屉', () => {
    const onClose = vi.fn()
    render(
      <DockedPanel open onClose={onClose} title="检查器" testId="t-dock">
        <input data-testid="t-input" defaultValue="编辑中" />
      </DockedPanel>,
    )
    fireEvent.keyDown(screen.getByTestId('t-input'), { key: 'Escape' })
    expect(onClose).not.toHaveBeenCalled()
    // 非输入目标才关
    fireEvent.keyDown(window, { key: 'Escape' })
    expect(onClose).toHaveBeenCalledTimes(1)
  })
})

describe('Inspector variant：默认 panel 零变更，docked 只出内容', () => {
  afterEach(() => cleanup())

  const baseProps = {
    batch: [] as never[],
    dirs: [] as { dir: string; count: number }[],
    busy: false,
    similarBusy: false,
    tagsVersion: 0,
    actions: {} as never,
    onPreview: () => {},
  }

  it('批量模式：panel 出「批量编辑 + N 张」标题行；docked 把标题/张数交给抽屉 header（不重复）', async () => {
    const { Inspector } = await import('../src/components/Inspector')
    const batch = [assetOf(ASSETS[0]), assetOf(ASSETS[1])] as never[]

    const panel = render(
      <ToastProvider>
        <Inspector asset={batch[0]} {...baseProps} batch={batch} />
      </ToastProvider>,
    )
    expect(screen.getByTestId('batch-count').textContent).toContain('2 张')
    expect(document.body.textContent).toContain('批量编辑')
    panel.unmount()

    render(
      <ToastProvider>
        <Inspector asset={batch[0]} variant="docked" {...baseProps} batch={batch} />
      </ToastProvider>,
    )
    // docked：没有自带标题行（避免和 DockedPanel header 的「批量编辑 / 已选 N 张」重复）
    expect(screen.queryByTestId('batch-count')).toBeNull()
    expect(document.body.textContent).not.toContain('批量编辑')
    // 但真内容（批量缩略图条 + 动作区）在（KI-11：批量打分区已移除，拒选仍在）
    expect(screen.getByTestId('batch-reject')).toBeTruthy()
    expect(document.body.textContent).not.toContain('评分')
  })

  it('默认（panel）：未选中仍是 48px 竖条；docked：未选中直接不渲染', async () => {
    const { Inspector } = await import('../src/components/Inspector')
    const panel = render(
      <ToastProvider>
        <Inspector asset={null} {...baseProps} />
      </ToastProvider>,
    )
    expect(screen.getByTestId('inspector-strip')).toBeTruthy()
    panel.unmount()

    render(
      <ToastProvider>
        <Inspector asset={null} variant="docked" {...baseProps} />
      </ToastProvider>,
    )
    expect(screen.queryByTestId('inspector-strip')).toBeNull()
    expect(screen.queryByTestId('inspector-empty')).toBeNull()
    expect(document.body.textContent).not.toContain('检查器')
  })
})

describe('InspectorDock：storage 不可用时的降级（隐私模式 / 配额满）', () => {
  afterEach(() => {
    cleanup()
    // 还原可用实现
    Object.defineProperty(globalThis, 'sessionStorage', { value: memStorage(), configurable: true, writable: true })
  })

  it('sessionStorage 抛错：自动弹出照旧、关闭不崩（不静默坏功能）', async () => {
    const boom = {
      getItem: () => {
        throw new Error('SecurityError')
      },
      setItem: () => {
        throw new Error('QuotaExceededError')
      },
      removeItem: () => {
        throw new Error('SecurityError')
      },
      clear: () => {},
      key: () => null,
      length: 0,
    } as unknown as Storage
    Object.defineProperty(globalThis, 'sessionStorage', { value: boom, configurable: true, writable: true })

    renderGallery('/gallery?ids=11')
    await settle()
    // 读标记失败 → 视为「没关过」→ 继续自动弹（不是静默永不弹）
    await waitDockOpen()
    // 写标记失败也不能让关闭/交互崩掉
    fireEvent.click(screen.getByTestId('inspector-dock-close'))
    await waitFor(() => expect(dock()).toBeNull())
  })
})
