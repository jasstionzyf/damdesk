// @vitest-environment jsdom
// @vitest-environment-options {"url": "http://localhost:3000/"}
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter, useLocation } from 'react-router-dom'
import { ToastProvider } from '../src/lib/toast'
import { TaskCenterProvider } from '../src/lib/tasks'
import { ConfirmProvider } from '../src/lib/confirm'
import { CommandPalette } from '../src/components/CommandPalette'
import { Gallery } from '../src/pages/Gallery'
import { Search } from '../src/pages/Search'
import { Settings } from '../src/pages/Settings'
import type { AssetView, SearchHit } from '../src/lib/api'

/**
 * KI-14（卡 task）：工具条只留**对选择集生效的批量动作**。
 *
 * 断言清单（对应卡片验收 1 / 4 / 5 / 7）：
 *  1. 图库工具条 DOM 项清单**精确等于**批量动作集（无 `similar-mode`、无 `DensityControl`）；
 *  2. 网格密度移到设置页「外观」可改（写偏好 `dam.gallery.thumb`），`?thumb=` 旧深链仍生效；
 *  3. 搜索页工具条同样只有批量动作（档位选择器在上传区旁，不在工具条）；
 *  4. 命令面板三条新命令可执行：找相似（语义）/ 找相似（精准）/ 网格密度。
 */

const _ls = new Map<string, string>()
Object.defineProperty(globalThis, 'localStorage', {
  configurable: true,
  writable: true,
  value: {
    getItem: (k: string) => _ls.get(k) ?? null,
    setItem: (k: string, v: string) => void _ls.set(k, String(v)),
    removeItem: (k: string) => void _ls.delete(k),
    clear: () => _ls.clear(),
    key: (i: number) => [..._ls.keys()][i] ?? null,
    get length() {
      return _ls.size
    },
  } satisfies Storage,
})
const _ss = new Map<string, string>()
Object.defineProperty(globalThis, 'sessionStorage', {
  configurable: true,
  writable: true,
  value: {
    getItem: (k: string) => _ss.get(k) ?? null,
    setItem: (k: string, v: string) => void _ss.set(k, String(v)),
    removeItem: (k: string) => void _ss.delete(k),
    clear: () => _ss.clear(),
    key: (i: number) => [..._ss.keys()][i] ?? null,
    get length() {
      return _ss.size
    },
  } satisfies Storage,
})

const asset = (id: number): AssetView =>
  ({
    assetId: id,
    mediaType: 'image',
    absPath: `/lib/a${id}.jpg`,
    storeMode: 'index',
    missing: 0,
    fileName: `a${id}.jpg`,
    fileExt: 'jpg',
    fileSz: 1024,
    mtime: 0,
    fileHash: null,
    titleCh: null,
    titleEn: null,
    description: null,
    source: 'import',
    parentAssetId: null,
    searchable: 1,
    deleted: 0,
    approvalStatus: 'none',
    starRating: 0,
    colorLabel: null,
    cloudAssetId: null,
    importedAt: id,
    embStatus: 'done',
    embModel: 'siglip2',
    dedupStatus: 'done',
    dedupModel: 'dinov3',
    thumbPath: null,
    width: 10,
    height: 10,
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
  }) as AssetView

const HITS: SearchHit[] = [
  {
    assetId: 101,
    resourceId: 1,
    score: 0.99,
    fileName: 'snow-1.jpg',
    titleCh: '雪山日出',
    thumbPath: null,
    width: 1200,
    height: 800,
  },
  {
    assetId: 102,
    resourceId: 2,
    score: 0.97,
    fileName: 'snow-2.jpg',
    titleCh: '雪山云海',
    thumbPath: null,
    width: 900,
    height: 1200,
  },
]

vi.mock('../src/lib/api', async (importOriginal) => {
  const actual = await importOriginal<Record<string, unknown>>()
  const stubs: Record<string, unknown> = {
    stats: async () => ({ assets: 3, totalBytes: 0, dirs: 1, tags: 0 }),
    dirs: async () => ({ dirs: [], roots: [] }),
    tags: async () => [],
    cameras: async () => ({ cameras: [] }),
    complianceVerdicts: async () => null,
    complianceReview: async () => ({ threshold: 0.7, items: [] }),
    assetTags: async () => [],
    complianceStatus: async () => ({ states: {}, pack: { id: 'none' }, verdictPackId: 'none' }),
    importStatus: async () => ({ running: false, progress: null }),
    capabilities: async () => ({
      isDesktop: false,
      storeMode: 'server-volume',
      writableRoots: [],
      roots: [],
      features: {},
      featureReasons: {},
    }),
    thumbUrl: (id: number) => `/api/media/thumb/${id}`,
    originalUrl: (id: number) => `/api/media/original/${id}`,
    assets: async (req?: { ids?: number[] }) =>
      req?.ids
        ? { items: req.ids.map((id) => asset(id)), total: req.ids.length }
        : { items: [asset(1), asset(2)], total: 2 },
    searchStatus: async () => ({
      searchable: true,
      activeModel: 'stub-embed',
      activeDims: 8,
      activeRows: 3,
      otherModelVectors: false,
      models: [],
      emb: { none: 0, pending: 0, done: 3, failed: 0 },
      semantic: { done: 3, remaining: 0, failed: 0 },
      similarMinScore: 0.72,
      similarMinScoreCalibrated: true,
    }),
    searchText: async () => ({ model: 'stub-embed', dims: 8, k: 24, count: HITS.length, hits: HITS }),
    searchSimilar: async () => ({ model: 'stub-embed', dims: 8, k: 24, count: 0, hits: [] }),
    searchImage: async () => ({ model: 'stub-embed', dims: 8, k: 24, count: 0, hits: [] }),
    smartFolders: async () => [],
    // ---- 设置页 ----
    inference: async () => ({
      settings: {
        active_embedder: 'siglip2-base-p224-onnx',
        model_base_url: '',
        auto_download: true,
        embedding: { base_url: '', api_key: '', model: '', dims: 0, timeout_secs: 1, proxy: '' },
        tag: {
          provider: 'damai',
          base_url: '',
          model: '',
          api_key: '',
          enabled: false,
          prompt_template: '',
          max_tokens: 2048,
          timeout_ms: 1,
          proxy: null,
        },
      },
      embedders: [
        {
          name: 'siglip2-base-p224-onnx',
          kind: 'embedding',
          dims: 768,
          modalities: ['image', 'text'],
          provider: 'onnx_siglip',
          preprocess: null,
        },
      ],
      imageOnlyEmbedders: [],
      capabilities: { localInfer: false, remoteProvider: true, note: '' },
    }),
    embedStatus: async () => null,
    embedFailed: async () => ({ items: [] }),
    embedStart: async () => ({ queued: 0, model: 'm' }),
    embedCancel: async () => ({ ok: true }),
    embedRetryFailed: async () => ({ reset: 0 }),
    tagPresets: async () => ({ presets: [] }),
    tagTest: async () => ({ ok: true }),
    tagScan: async () => ({ models: [] }),
    exifSettings: async () => null,
    exifStatus: async () => null,
  }
  const api = new Proxy(stubs, {
    get(target, key: string) {
      if (key in target) return target[key]
      return async () => ({})
    },
  })
  return { ...actual, api }
})

function LocationProbe() {
  const loc = useLocation()
  return <span data-testid="loc">{loc.pathname + loc.search}</span>
}

/** 某个容器里的全部 data-testid（排序后用于「项清单」对账） */
function testidsWithin(el: HTMLElement): string[] {
  return Array.from(el.querySelectorAll('[data-testid]'))
    .map((e) => e.getAttribute('data-testid') ?? '')
    .sort()
}

function renderGallery(initial: string) {
  return render(
    <ToastProvider>
      <TaskCenterProvider>
        <ConfirmProvider>
          <MemoryRouter initialEntries={[initial]}>
            <LocationProbe />
            <Gallery onImport={() => {}} />
          </MemoryRouter>
        </ConfirmProvider>
      </TaskCenterProvider>
    </ToastProvider>,
  )
}

function renderSearch(initial: string) {
  return render(
    <ToastProvider>
      <TaskCenterProvider>
        <ConfirmProvider>
          <MemoryRouter initialEntries={[initial]}>
            <LocationProbe />
            <Search />
          </MemoryRouter>
        </ConfirmProvider>
      </TaskCenterProvider>
    </ToastProvider>,
  )
}

beforeAll(() => {
  Object.defineProperty(window, 'matchMedia', {
    writable: true,
    value: (query: string) => ({
      matches: true,
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    }),
  })
  Object.defineProperty(window, 'innerWidth', { writable: true, configurable: true, value: 1600 })
  Object.defineProperty(window, 'innerHeight', { writable: true, configurable: true, value: 1000 })
  ;(globalThis as unknown as { ResizeObserver: unknown }).ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  }
  // jsdom 没有 scrollIntoView；cmdk 选中/过滤时会调用
  Element.prototype.scrollIntoView = () => {}
})

beforeEach(() => {
  _ls.clear()
  _ss.clear()
  window.history.replaceState(null, '', '/')
})

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
})

describe('KI-14 图库工具条：只留批量动作', () => {
  it('批量动作 = 共用 SelectionBar；工具条只剩「快捷键 / 详情」两个常驻门', async () => {
    renderGallery('/?ids=1')
    const bar = await waitFor(() => screen.getByTestId('grid-toolbar'))
    const toolIds = testidsWithin(bar)
    // 常驻门：快捷键速查 + 检查器开关（不是批量动作，也不承担其他能力）
    expect(toolIds).toEqual(['help-button', 'inspector-toggle'])

    // 批量动作挂在共用 SelectionBar（与搜索/合规同一份实现，禁止各页自拼）
    const selBar = await waitFor(() => screen.getByTestId('selection-bar'))
    const selIds = testidsWithin(selBar)
    expect(selIds).toEqual([
      'delete-button',
      'export-button',
      'gallery-batch-hint',
      'migrate-button',
      'reject-button',
      'selection-clear',
      'selection-count',
      'selection-loaded',
      'selection-select-all',
      // KI35-P3：能力入口「送去处理」与批量动作同一份 SelectionBar（打开能力面板）
      'verb-send',
    ])
    // 整页工具条区（常驻门 + 批量动作）里不得再出现参数开关/单图动作
    const all = [...toolIds, ...selIds]
    expect(all).not.toContain('similar-mode')
    expect(all.some((t) => t.startsWith('density'))).toBe(false)
    expect(all).not.toContain('insp-find-similar')
    expect(screen.queryByTestId('similar-mode')).toBeNull()
    expect(screen.queryByTestId('density-control')).toBeNull()
    console.log('[图库 常驻门]', toolIds.join(', '), '｜[SelectionBar]', selIds.join(', '))
  })

  it('?thumb= 旧深链仍生效（密度仍走 URL 真源，只是控件搬走了）', async () => {
    renderGallery('/?ids=1&thumb=130')
    const grid = await waitFor(() => screen.getByTestId('asset-grid-root'))
    expect(grid.getAttribute('data-thumb')).toBe('130')
    expect(screen.queryByTestId('density-control')).toBeNull()
  })
})

describe('KI-14 网格密度：设置页「外观」可改', () => {
  it('设置页有外观区 + 同一份 DensityControl；点 L 写偏好 dam.gallery.thumb=220', async () => {
    render(
      <ToastProvider>
        <TaskCenterProvider>
          <ConfirmProvider>
            <MemoryRouter>
              <Settings />
            </MemoryRouter>
          </ConfirmProvider>
        </TaskCenterProvider>
      </ToastProvider>,
    )
    const section = await waitFor(() => screen.getByTestId('settings-appearance'), { timeout: 6000 })
    expect(section.textContent).toContain('外观')
    expect(section.textContent).toContain('网格密度')
    const ctrl = await waitFor(() => screen.getByTestId('density-control'))
    expect(ctrl.getAttribute('data-thumb')).toBe('170')
    fireEvent.click(screen.getByTestId('density-L'))
    await waitFor(() => expect(_ls.get('dam.gallery.thumb')).toBe('220'))
    expect(screen.getByTestId('density-control').getAttribute('data-thumb')).toBe('220')
  })
})

describe('KI-14 搜索页：工具条同样只留批量动作', () => {
  // 选择条由 task 收敛成共用 SelectionBar（testid selection-*），批量动作
  // （打标/导出/详情门）也收进同一条（KI-16 #5：搜索与图库同一份 SelectionBar），
  // 不再有独立的 search-batch-bar —— 只断言「只有批量动作、没有参数开关」这条不变量。
  const SELECTION_BAR_IDS = new Set([
    'selection-bar',
    'selection-count',
    'selection-loaded',
    'selection-select-all',
    'selection-clear',
    'selection-hint',
    'batch-open-gallery',
    'batch-tag-open',
    'batch-export',
    'search-inspector-toggle',
  ])

  it('结果选择条只含批量项；页面不出现被收敛的开关与密度控件', async () => {
    renderSearch('/search?q=%E9%9B%AA%E5%B1%B1')
    const selBar = await waitFor(() => {
      const el = screen.queryByTestId('selection-bar')
      if (!el) throw new Error('选择条还没渲染')
      return el
    })
    let selIds = testidsWithin(selBar)
    // 只含批量/选择项（口径见 SelectionBar 契约）
    for (const t of selIds) expect(SELECTION_BAR_IDS.has(t)).toBe(true)
    // 必须能「全选」和「清空」
    expect(selIds).toContain('selection-select-all')
    expect(selIds).toContain('selection-clear')

    // 全选后批量动作出现在**同一条**里：打标 / 导出 / 详情（详情是抽屉门）；
    // 独立 search-batch-bar 已随 KI-16 #5 退役
    fireEvent.click(screen.getByTestId('selection-select-all'))
    await waitFor(() => expect(screen.getByTestId('batch-tag-open')).toBeTruthy())
    selIds = testidsWithin(screen.getByTestId('selection-bar'))
    expect(selIds).toContain('batch-tag-open')
    expect(selIds).toContain('batch-export')
    expect(selIds).toContain('search-inspector-toggle')
    expect(screen.queryByTestId('search-batch-bar')).toBeNull()
    // 参数开关与密度控件不得出现在搜索页任何工具条
    expect(selIds).not.toContain('similar-mode')
    expect(selIds.some((t) => t.startsWith('density'))).toBe(false)
    expect(screen.queryByTestId('density-control')).toBeNull()
    console.log('[搜索页 选择条]', selIds.join(', '))
  })

  it('「以图搜图」上传区旁带同一档位选择器（文案与检查器菜单一致、写同一偏好）', async () => {
    renderSearch('/search?mode=image')
    const group = await waitFor(() => screen.getByTestId('search-similar-mode'))
    expect(group.textContent).toContain('语义相似')
    expect(group.textContent).toContain('精准匹配')
    expect(group.textContent).toContain('同类的其它图')
    fireEvent.click(screen.getByTestId('search-similar-mode-visual_precise'))
    await waitFor(() => expect(_ls.get('dam.similarMode')).toBe('visual_precise'))
    expect(screen.getByTestId('search-similar-mode-note').textContent).toContain('同一张图的不同版本')
  })
})

describe('KI-14 命令面板：三个新入口可执行', () => {
  const renderPalette = () =>
    render(
      <ToastProvider>
        <TaskCenterProvider>
          <MemoryRouter initialEntries={['/settings']}>
            <LocationProbe />
            <CommandPalette open onClose={() => {}} onOpenTaskCenter={() => {}} />
          </MemoryRouter>
        </TaskCenterProvider>
      </ToastProvider>,
    )

  const clickItem = (label: string) => {
    const el = screen.getByText(label)
    const item = el.closest('[cmdk-item]') as HTMLElement | null
    fireEvent.click(item ?? el)
  }

  it('找相似（语义）→ 图库深链 find=similar&simMode=semantic', async () => {
    renderPalette()
    await waitFor(() => expect(screen.getByText('找相似（语义）：同类的其它图')).toBeTruthy())
    clickItem('找相似（语义）：同类的其它图')
    await waitFor(() => expect(screen.getByTestId('loc').textContent).toContain('find=similar'))
    expect(screen.getByTestId('loc').textContent).toContain('simMode=semantic')
  })

  it('找相似（精准）→ 图库深链 simMode=visual_precise', async () => {
    renderPalette()
    await waitFor(() => expect(screen.getByText('找相似（精准）：同一张图的不同版本')).toBeTruthy())
    clickItem('找相似（精准）：同一张图的不同版本')
    await waitFor(() => expect(screen.getByTestId('loc').textContent).toContain('simMode=visual_precise'))
    expect(screen.getByTestId('loc').textContent).toContain('find=similar')
  })

  it('网格密度 → 图库深链带 thumb=，并写同一偏好（S→M 默认起步）', async () => {
    renderPalette()
    await waitFor(() => expect(screen.getByText('网格密度：切换 小 / 中 / 大')).toBeTruthy())
    clickItem('网格密度：切换 小 / 中 / 大')
    await waitFor(() => expect(screen.getByTestId('loc').textContent).toContain('thumb='))
    // 默认档 M(170) 的下一档是 L(220)；偏好与 URL 同步
    expect(screen.getByTestId('loc').textContent).toContain('thumb=220')
    expect(_ls.get('dam.gallery.thumb')).toBe('220')
    console.log('[命令面板跳转]', screen.getByTestId('loc').textContent)
  })
})
