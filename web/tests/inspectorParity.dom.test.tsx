// @vitest-environment jsdom
// @vitest-environment-options {"url": "http://localhost:3000/gallery"}
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { BrowserRouter } from 'react-router-dom'
import { ToastProvider } from '../src/lib/toast'
import { ConfirmProvider } from '../src/lib/confirm'

/**
 * KI-12（卡 t_8a2d5d98）：图库与搜索的图片详情面统一 —— 只保留 InspectorDock 一个组件。
 *
 * 验收口径：同一资产在图库与搜索打开的 Inspector **字段逐项一致**（属性/标签/目录/合规/操作区）。
 * 本测试跑两条**真实页面挂载**（不是只渲染共享组件 —— 那样恒等，测不出回归）：
 *   A. /gallery?ids=11（选择集深链，与真实刷新/分享同路径）→ 检查器抽屉；
 *   B. /search?q=雪山 → 点结果卡选中 → 检查器抽屉（KI-12 新挂的 InspectorDock）。
 * 两处各收集「dt 字段名全集 + data-testid 全集」，断言完全一致，并断言规范字段都在。
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

Object.defineProperty(globalThis, 'localStorage', { value: memStorage(), configurable: true, writable: true })
Object.defineProperty(globalThis, 'sessionStorage', { value: memStorage(), configurable: true, writable: true })

const ASSETS = [
  { assetId: 11, fileName: 'snow-1.jpg', titleCh: '雪山日出' },
  { assetId: 12, fileName: 'snow-2.jpg', titleCh: '雪山云海' },
  { assetId: 13, fileName: 'beach.jpg', titleCh: '海滩' },
]

const SEARCH_HITS = [
  { assetId: 101, resourceId: 1, score: 0.991, fileName: 'snow-1.jpg', titleCh: '雪山日出', thumbPath: null, width: 1200, height: 800 },
]

function assetOf(opts: { assetId: number; fileName: string; titleCh: string | null; width?: number; height?: number }) {
  return {
    assetId: opts.assetId,
    mediaType: 'image',
    absPath: `/library/${opts.fileName}`,
    storeMode: 'index',
    missing: 0,
    fileName: opts.fileName,
    fileExt: 'jpg',
    fileSz: 2048,
    mtime: 0,
    fileHash: null,
    titleCh: opts.titleCh,
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
    width: opts.width ?? 1200,
    height: opts.height ?? 800,
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

const galleryAsset = assetOf(ASSETS[0])
const searchAsset = assetOf({ assetId: 101, fileName: 'snow-1.jpg', titleCh: '雪山日出', width: 1200, height: 800 })

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
  assetTagResults: vi.fn(),
  smartFolder: vi.fn(),
  smartFolderDelete: vi.fn(),
  searchStatus: vi.fn(),
  searchText: vi.fn(),
  searchSimilar: vi.fn(),
  searchImage: vi.fn(),
  reject: vi.fn(),
  addTags: vi.fn(),
  removeTag: vi.fn(),
  setColor: vi.fn(),
  moveAssets: vi.fn(),
  thumbUrl: (id: number, size: number = 256) => `/api/media/thumb/${id}?size=${size}`,
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

// 搜索页的任务中心只用来起嵌入：整块桩掉，避免测试里拉起全局轮询
vi.mock('../src/lib/tasks', () => ({
  useTasks: () => ({ startEmbed: vi.fn() }),
  TaskCenterProvider: ({ children }: { children: React.ReactNode }) => children,
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
apiStub.dirs.mockImplementation(async () => ({ dirs: [{ dir: '/library', count: 3 }], roots: ['/library'] }))
apiStub.tags.mockImplementation(async () => [])
apiStub.complianceStatus.mockImplementation(async () => ({
  states: { pass: 0, fail: 0, unknown: 0, pending: 0, exempt: 0 },
  pack: { id: 'stub' },
}))
apiStub.importStatus.mockImplementation(async () => ({ running: false, progress: null }))
apiStub.assets.mockImplementation(async (q: { ids?: number[] }) => {
  const ids = q.ids ?? ASSETS.map((a) => a.assetId)
  const items = ids.map((id) => {
    const hit = SEARCH_HITS.find((h) => h.assetId === id)
    if (hit) return searchAsset
    const base = ASSETS.find((a) => a.assetId === id)
    return base ? galleryAsset : galleryAsset
  })
  return { total: items.length, items }
})
apiStub.capabilities.mockImplementation(async () => ({ features: {} }))
apiStub.assetTags.mockImplementation(async () => [])
apiStub.assetTagResults.mockImplementation(async () => [])
apiStub.exportStatus.mockImplementation(async () => ({ running: false, progress: null, report: null, error: null }))
apiStub.appInfo.mockImplementation(async () => ({ dataDir: '/data' }))
apiStub.cameras.mockImplementation(async () => ({ cameras: [] }))
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
apiStub.ocrAssetText.mockImplementation(async (assetId: number) => ({ assetId, status: 'none', text: null, blocks: [] }))
apiStub.complianceReview.mockImplementation(async () => ({ items: [], total: 0, nextCursor: null }))
apiStub.searchStatus.mockImplementation(async () => ({
  searchable: true,
  activeModel: 'stub-embed',
  activeDims: 8,
  activeRows: 3,
  otherModelVectors: false,
  emb: { none: 0 },
}))
apiStub.searchText.mockImplementation(async () => ({
  model: 'stub-embed',
  dims: 8,
  k: 24,
  count: SEARCH_HITS.length,
  hits: SEARCH_HITS,
}))
apiStub.searchSimilar.mockImplementation(async () => ({ model: 'stub-embed', dims: 8, k: 24, count: 0, hits: [] }))
apiStub.searchImage.mockImplementation(async () => ({ model: 'stub-embed', dims: 8, k: 24, count: 0, hits: [] }))

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

/** 抽屉内容签名：dt 字段名全集 + data-testid 全集（同一组件 → 两处挂载必须完全一致） */
function dockSignature(): { fields: string[]; testids: string[] } {
  const dock = screen.getByTestId('inspector-dock')
  const fields = Array.from(dock.querySelectorAll('dt')).map((el) => el.textContent ?? '')
  const testids = Array.from(dock.querySelectorAll('[data-testid]')).map((el) =>
    el.getAttribute('data-testid') ?? '',
  )
  return { fields: [...fields].sort(), testids: [...testids].sort() }
}

async function waitDockContent() {
  await waitFor(() => {
    expect(screen.getByTestId('inspector-dock')).toBeTruthy()
    const text = screen.getByTestId('inspector-dock').textContent ?? ''
    // 属性区与操作区都已渲染（单选模式）
    expect(text).toContain('尺寸')
    expect(text).toContain('找相似的')
  })
}

describe('KI-12 图库与搜索的检查器字段逐项一致', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    sessionStorage.clear()
  })
  afterEach(() => cleanup())

  it('同一资产：/gallery 深链与 /search 选中打开的 Inspector 字段/testid 完全一致', async () => {
    // ---- A. 图库：?ids=11 深链（真实刷新/分享路径） ----
    window.history.replaceState(null, '', '/gallery?ids=11')
    const { Gallery } = await import('../src/pages/Gallery')
    const galleryRender = render(
      <BrowserRouter>
        <ToastProvider>
          <ConfirmProvider>
            <Gallery onImport={() => {}} />
          </ConfirmProvider>
        </ToastProvider>
      </BrowserRouter>,
    )
    await waitFor(() => expect(screen.getByTestId('inspector-dock')).toBeTruthy())
    await waitDockContent()
    const gallerySig = dockSignature()
    galleryRender.unmount()
    cleanup()

    // ---- B. 搜索：q=雪山 自动检索 → 点结果卡 → 抽屉自动滑出（KI-12 新挂载点） ----
    window.history.replaceState(null, '', '/search?q=%E9%9B%AA%E5%B1%B1')
    const { Search } = await import('../src/pages/Search')
    const searchRender = render(
      <BrowserRouter>
        <ToastProvider>
          <ConfirmProvider>
            <Search />
          </ConfirmProvider>
        </ToastProvider>
      </BrowserRouter>,
    )
    await waitFor(() => expect(screen.getByTestId('asset-card-101')).toBeTruthy())
    expect(screen.queryByTestId('inspector-dock')).toBeNull() // 默认收起：未选中无抽屉
    fireEvent.click(screen.getByTestId('asset-card-101'))
    await waitFor(() => expect(screen.getByTestId('inspector-dock')).toBeTruthy())
    await waitDockContent()
    const searchSig = dockSignature()
    searchRender.unmount()

    // ---- 断言：字段逐项一致 ----
    expect(searchSig.fields).toEqual(gallerySig.fields)
    expect(searchSig.testids).toEqual(gallerySig.testids)
    // 规范字段（验收清单：属性/标签/目录/操作区）都在 —— 两处同一组件的最低要求
    for (const field of ['尺寸', '大小', '格式', '导入时间', '存放', '状态', 'ID']) {
      expect({ field, inGallery: gallerySig.fields.includes(field), inSearch: searchSig.fields.includes(field) })
        .toEqual({ field, inGallery: true, inSearch: true })
    }
    for (const tid of ['tag-input', 'tag-add', 'insp-colors', 'move-select', 'move-apply', 'insp-find-similar']) {
      expect({ tid, inGallery: gallerySig.testids.includes(tid), inSearch: searchSig.testids.includes(tid) })
        .toEqual({ tid, inGallery: true, inSearch: true })
    }
    // DECouple-FE（卡 t_ad156948）：检查器是**纯资产域** —— 判定/合规区块已整体下线
    for (const tid of ['insp-compliance', 'insp-compliance-state', 'insp-compliance-asset-review']) {
      expect(gallerySig.testids).not.toContain(tid)
      expect(searchSig.testids).not.toContain(tid)
    }
  })

  it('DECouple-FE：检查器不再有外链「打开原图」（Tauri 丢弃 target=_blank，点击无反应）', async () => {
    window.history.replaceState(null, '', '/gallery?ids=11')
    const { Gallery } = await import('../src/pages/Gallery')
    const r = render(
      <BrowserRouter>
        <ToastProvider>
          <ConfirmProvider>
            <Gallery onImport={() => {}} />
          </ConfirmProvider>
        </ToastProvider>
      </BrowserRouter>,
    )
    await waitFor(() => expect(screen.getByTestId('inspector-dock')).toBeTruthy())
    await waitDockContent()
    const dock = screen.getByTestId('inspector-dock')
    // 一个能力一个入口：看原图 = 「预览大图」（应用内浮层），不再有第二个外链入口
    expect(dock.textContent).not.toContain('打开原图')
    expect(dock.querySelector('a[target="_blank"]')).toBeNull()
    expect(dock.textContent).toContain('预览大图')
    r.unmount()
  })

  it('搜索页未选中时不渲染抽屉（默认收起），选中后自动展开', async () => {
    window.history.replaceState(null, '', '/search?q=%E9%9B%AA%E5%B1%B1')
    const { Search } = await import('../src/pages/Search')
    const r = render(
      <BrowserRouter>
        <ToastProvider>
          <ConfirmProvider>
            <Search />
          </ConfirmProvider>
        </ToastProvider>
      </BrowserRouter>,
    )
    await waitFor(() => expect(screen.getByTestId('asset-card-101')).toBeTruthy())
    expect(screen.queryByTestId('inspector-dock')).toBeNull()
    fireEvent.click(screen.getByTestId('asset-card-101'))
    await waitFor(() => expect(screen.getByTestId('inspector-dock')).toBeTruthy())
    r.unmount()
  })
})
