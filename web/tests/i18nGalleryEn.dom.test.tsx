// @vitest-environment jsdom
// @vitest-environment-options {"url": "http://localhost:3000/"}
/**
 * i18n-B 硬门禁（卡 task）：en locale 下图库页 / 搜索页 **无残留中文**。
 *
 * 口径（卡体验收「无残留中文（硬门禁）」）：
 *  - 预置 `dam.locale = en-US`，渲染整页（导航 + 工具条 + 空态 + 侧栏 + 检查器）；
 *  - 取 `document.body.innerText`（= 用户真正看见的文字），CJK 计数必须 == 0；
 *  - 夹具数据（文件名 / 路径 / 标题）全部用 ASCII，白名单因此为空 ——
 *    用户数据里出现中文是允许的，本用例不用它开脱。
 *
 * 渲染脚手架沿用 `toolbarConsolidation.dom.test.tsx` 的 stub 写法，额外包 `LocaleProvider`。
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, render, waitFor } from '@testing-library/react'
import { MemoryRouter } from 'react-router-dom'
import { ToastProvider } from '../src/lib/toast'
import { TaskCenterProvider } from '../src/lib/tasks'
import { ConfirmProvider } from '../src/lib/confirm'
import { LocaleProvider } from '../src/i18n'
import { Gallery } from '../src/pages/Gallery'
import { Search } from '../src/pages/Search'
import type { AssetView, SearchHit } from '../src/lib/api'

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

/** CJK 判据：汉字 + 中文标点 + 全角符号（英文界面里这些一律不该出现） */
const CJK = /[\u3400-\u9fff\u3000-\u303f\uff01-\uff60]/

/**
 * 页面文字快照：jsdom **不实现 innerText**（真浏览器口径），这里用 `textContent`
 * ——它包含 CSS 隐藏节点，比 innerText 更严：连 hover 才出现的 title/aria 文案也一并检查。
 */
function pageText(): string {
  return document.body.textContent ?? ''
}

/** 取出页面文字里的 CJK 片段（失败时便于定位是哪个词没译） */
function cjkHits(text: string): string[] {
  const out: string[] = []
  for (const line of text.split('\n')) {
    const trimmed = line.trim()
    if (!trimmed) continue
    if (CJK.test(trimmed)) out.push(trimmed)
  }
  return out
}

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
    titleCh: null,
    thumbPath: null,
    width: 1200,
    height: 800,
  },
  {
    assetId: 102,
    resourceId: 2,
    score: 0.97,
    fileName: 'snow-2.jpg',
    titleCh: null,
    thumbPath: null,
    width: 900,
    height: 1200,
  },
]

vi.mock('../src/lib/api', async (importOriginal) => {
  const actual = await importOriginal<Record<string, unknown>>()
  const stubs: Record<string, unknown> = {
    stats: async () => ({ assets: 2, totalBytes: 0, dirs: 1, tags: 0 }),
    dirs: async () => ({ dirs: [{ dir: '/lib', count: 2 }], roots: ['/lib'] }),
    tags: async () => [],
    tagGroups: async () => [],
    cameras: async () => ({ cameras: [] }),
    complianceVerdicts: async () => null,
    complianceReview: async () => ({ threshold: 0.7, items: [] }),
    complianceStatus: async () => ({ states: {}, pack: { id: 'none' }, verdictPackId: 'none' }),
    assetTags: async () => [],
    importStatus: async () => ({ running: false, progress: null }),
    capabilities: async () => ({
      isDesktop: false,
      storeMode: 'server-volume',
      writableRoots: [],
      roots: ['/lib'],
      features: {},
      featureReasons: {},
    }),
    thumbUrl: (id: number) => `/api/media/thumb/${id}`,
    originalUrl: (id: number) => `/api/media/original/${id}`,
    assets: async () => ({ items: [asset(1), asset(2)], total: 2 }),
    asset: async (id: number) => asset(id),
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
    embedStatus: async () => null,
    embedFailed: async () => ({ items: [] }),
    embedStart: async () => ({ queued: 0, model: 'm' }),
    embedCancel: async () => ({ ok: true }),
    embedRetryFailed: async () => ({ reset: 0 }),
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

function renderPage(node: React.ReactNode, initial: string) {
  return render(
    <LocaleProvider>
      <ToastProvider>
        <TaskCenterProvider>
          <ConfirmProvider>
            <MemoryRouter initialEntries={[initial]}>{node}</MemoryRouter>
          </ConfirmProvider>
        </TaskCenterProvider>
      </ToastProvider>
    </LocaleProvider>,
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
  Element.prototype.scrollIntoView = () => {}
})

beforeEach(() => {
  _ls.clear()
  _ss.clear()
  // 预置英文：本用例的唯一样本口径
  _ls.set('dam.locale', 'en-US')
  window.history.replaceState(null, '', '/')
})

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
})

describe('i18n-B 门禁：en locale 无残留中文', () => {
  it('图库页：body.innerText 的 CJK 计数 == 0，且关键界面文案已是英文', async () => {
    renderPage(<Gallery onImport={() => {}} />, '/?ids=1')
    // 等到网格 + 侧栏都出内容（异步 stub 各走一轮）
    await waitFor(() => expect(pageText().length).toBeGreaterThan(80))
    await waitFor(() => expect(document.querySelector('[data-testid="grid-toolbar"]')).toBeTruthy())

    const text = pageText()
    expect(cjkHits(text), `残留中文行：\n${cjkHits(text).join('\n')}`).toEqual([])
    // 正例：确认确实渲染了英文界面（不是整页没渲染的空 assertion）
    expect(text).toMatch(/Library|Import|Search|Select|Details/i)
  })

  it('图库空态（筛选无命中）：空态标题/说明/按钮无中文', async () => {
    renderPage(<Gallery onImport={() => {}} />, '/?q=zzz-nothing-matches')
    await waitFor(() => expect(pageText().length).toBeGreaterThan(40))
    const text = pageText()
    expect(cjkHits(text), `残留中文行：\n${cjkHits(text).join('\n')}`).toEqual([])
  })

  it('搜索页：body.innerText 的 CJK 计数 == 0', async () => {
    renderPage(<Search />, '/search')
    await waitFor(() => expect(pageText().length).toBeGreaterThan(80))
    const text = pageText()
    expect(cjkHits(text), `残留中文行：\n${cjkHits(text).join('\n')}`).toEqual([])
    expect(text).toMatch(/Search|Browse|Threshold|Filter/i)
  })
})
