// @vitest-environment jsdom
// @vitest-environment-options {"url": "http://localhost:3000/"}
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { BrowserRouter, MemoryRouter, useLocation } from 'react-router-dom'
import { ToastProvider } from '../src/lib/toast'
import { TaskCenterProvider } from '../src/lib/tasks'
import { ConfirmProvider } from '../src/lib/confirm'
import type { AssetView } from '../src/lib/api'

/**
 * KI-14（卡 t_a0350b71）：「找相似的」split button —— 档位搬家后的唯一入口。
 *
 * 承接原 ki5SimilarMode.dom.test.tsx 的全部有效断言（精准接口/分带/缺向量/横幅），并新增：
 *  1. 主键 = 按当前档位执行（默认语义；偏好档为精准时主键就是精准）；
 *  2. caret 菜单两项带人话说明（语义相似 = 同类的其它图 / 精准匹配 = 同一张图的不同版本），
 *     选档 = 写偏好 + 立即以新档执行（参数选择必须有可理解的后果，KI-14 根因）；
 *  3. 工具条不再出现模式开关：切档不写 URL（`?simMode=` 只兼容旧深链，只读）；
 *  4. 两档请求体对账（真 api 层 + fetch 桩）：body.mode = semantic / visual_precise。
 */

// jsdom 在本仓 vitest 环境里不提供可用的 localStorage（Gallery 首屏会读偏好）——给最小 Storage 桩
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

const calls = vi.hoisted(() => ({
  searchSimilar: vi.fn(),
  searchSimilarPrecise: vi.fn(),
  assets: vi.fn(),
}))

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
    capabilities: async () => ({ features: {} }),
    thumbUrl: (id: number) => `/api/media/thumb/${id}`,
    originalUrl: (id: number) => `/api/media/original/${id}`,
    assets: calls.assets,
    searchSimilar: calls.searchSimilar,
    searchSimilarPrecise: calls.searchSimilarPrecise,
  }
  const api = new Proxy(stubs, {
    get(target, key: string) {
      if (key in target) return target[key]
      return async () => ({})
    },
  })
  return { ...actual, api }
})

/** 把当前 URL 渲染出来，便于断言「切档不写 URL」 */
function LocationProbe() {
  const loc = useLocation()
  return <span data-testid="loc">{loc.pathname + loc.search}</span>
}

async function renderGallery(initial: string) {
  const { Gallery } = await import('../src/pages/Gallery')
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

/** 旧深链用例要走真实地址（similarMode 的 `?simMode=` 兼容读的是 window.location） */
async function renderGalleryBrowser(initial: string) {
  window.history.replaceState(null, '', initial)
  const { Gallery } = await import('../src/pages/Gallery')
  return render(
    <BrowserRouter>
      <ToastProvider>
        <TaskCenterProvider>
          <ConfirmProvider>
            <Gallery onImport={() => {}} />
          </ConfirmProvider>
        </TaskCenterProvider>
      </ToastProvider>
    </BrowserRouter>,
  )
}

const PRECISE_RESULT = {
  model: 'siglip2-base-p224-onnx',
  dims: 768,
  k: 60,
  count: 2,
  mode: 'visual_precise',
  hits: [
    {
      assetId: 2,
      resourceId: 2,
      score: 0.995,
      band: 'near_duplicate',
      fileName: 'a2.jpg',
      titleCh: null,
      thumbPath: null,
      width: 10,
      height: 10,
      semanticScore: 0.81,
    },
    {
      assetId: 3,
      resourceId: 3,
      score: 0.93,
      band: 'variant',
      fileName: 'a3.jpg',
      titleCh: null,
      thumbPath: null,
      width: 10,
      height: 10,
      semanticScore: 0.55,
    },
  ],
  candidatePool: 3,
  missingDedupVectors: 1,
  queryMissingDedupVector: false,
  dedupIndexReady: true,
  nearThreshold: 0.98,
  variantThreshold: 0.9,
  selfExcluded: true,
}

const SEMANTIC_RESULT = {
  model: 'siglip2-base-p224-onnx',
  dims: 768,
  k: 60,
  count: 1,
  hits: [
    {
      assetId: 2,
      resourceId: 2,
      score: 0.93,
      fileName: 'a2.jpg',
      titleCh: null,
      thumbPath: null,
      width: 10,
      height: 10,
    },
  ],
  minScore: 0.72,
  minScoreCalibrated: true,
}

const DOCK_WAIT = { timeout: 4000 }

/** 检查器抽屉是「选中即自动滑出」的异步链（useInspectorDock → DockedPanel），等它落定 */
async function waitMain(): Promise<HTMLElement> {
  return waitFor(() => screen.getByTestId('insp-find-similar'), DOCK_WAIT)
}

async function openCaretAndPick(option: 'semantic' | 'visual_precise') {
  fireEvent.click(await waitFor(() => screen.getByTestId('insp-find-similar-caret'), DOCK_WAIT))
  await waitFor(() => expect(screen.getByTestId('insp-find-similar-menu')).toBeTruthy(), DOCK_WAIT)
  await act(async () => {
    fireEvent.click(screen.getByTestId(`similar-option-${option}`))
  })
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
})

beforeEach(() => {
  _ls.clear()
  _ss.clear()
  window.history.replaceState(null, '', '/')
  calls.assets.mockResolvedValue({ items: [asset(1), asset(2), asset(3)], total: 3 })
  calls.searchSimilar.mockResolvedValue(SEMANTIC_RESULT)
  calls.searchSimilarPrecise.mockResolvedValue(PRECISE_RESULT)
})

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
})

describe('KI-14 找相似 split button', () => {
  it('主键 = 按当前档位（默认语义）执行：split 结构存在、菜单未展开', async () => {
    await renderGallery('/?ids=1')
    const main = await waitMain()
    expect(main.textContent).toContain('找相似的')
    expect(main.textContent).toContain('语义相似')
    expect(screen.getByTestId('insp-find-similar-caret')).toBeTruthy()
    expect(screen.queryByTestId('insp-find-similar-menu')).toBeNull()

    await act(async () => {
      fireEvent.click(main)
    })
    await waitFor(() => expect(screen.getByTestId('similar-banner')).toBeTruthy(), DOCK_WAIT)
    // 语义档：显式 mode=semantic；精准接口一次都不调
    expect(calls.searchSimilar).toHaveBeenCalledWith(1, 60, undefined, undefined, 'semantic')
    expect(calls.searchSimilarPrecise).not.toHaveBeenCalled()
    expect(screen.getByTestId('similar-banner').textContent).toContain('来自：找相似 · 语义相似')
    // KI-14：档位不写 URL
    expect(screen.getByTestId('loc').textContent).not.toContain('simMode')
  })

  it('caret 菜单两项带人话说明；选中项有 ✓（aria-checked）', async () => {
    await renderGallery('/?ids=1')
    fireEvent.click(await waitFor(() => screen.getByTestId('insp-find-similar-caret'), DOCK_WAIT))
    await waitFor(() => expect(screen.getByTestId('insp-find-similar-menu')).toBeTruthy())
    const menu = screen.getByTestId('insp-find-similar-menu')
    const sem = screen.getByTestId('similar-option-semantic')
    const pre = screen.getByTestId('similar-option-visual_precise')
    expect(menu.textContent).toContain('语义相似')
    expect(menu.textContent).toContain('同类的其它图')
    expect(menu.textContent).toContain('精准匹配')
    expect(menu.textContent).toContain('同一张图的不同版本')
    expect(sem.getAttribute('aria-checked')).toBe('true')
    expect(pre.getAttribute('aria-checked')).toBe('false')
  })

  it('菜单选「精准匹配」→ 写偏好 + 立即执行精准接口；横幅报模式/分带/缺向量，语义接口一次都不调', async () => {
    await renderGallery('/?ids=1')
    await waitFor(() => screen.getByTestId('insp-find-similar-caret'), DOCK_WAIT)
    await openCaretAndPick('visual_precise')

    await waitFor(() => expect(screen.getByTestId('similar-banner')).toBeTruthy(), DOCK_WAIT)
    expect(calls.searchSimilarPrecise).toHaveBeenCalledWith(1, 60)
    expect(calls.searchSimilar).not.toHaveBeenCalled()
    // 档位写进偏好（localStorage），不写 URL
    expect(_ls.get('dam.similarMode')).toBe('visual_precise')
    expect(screen.getByTestId('loc').textContent).not.toContain('simMode')
    // 跑完临时集合会清空选择集（抽屉随之收起）—— 同一偏好下重新挂载时主键应已是精准档
    // （档位真的持久化了，不是只改了一次性入参）
    cleanup()
    await renderGallery('/?ids=1')
    const main = await waitMain()
    expect(main.textContent).toContain('精准匹配')
  })

  it('切回「语义相似」→ 请求与结果都变（同一张图：精准 → 语义，分带消失）', async () => {
    // 第一轮：偏好已是精准（上一轮持久化的档位）→ 新挂载直接按精准档执行
    _ls.set('dam.similarMode', 'visual_precise')
    await renderGallery('/?ids=1')
    const main = await waitMain()
    expect(main.textContent).toContain('精准匹配')
    await act(async () => {
      fireEvent.click(main)
    })
    await waitFor(() => expect(screen.getByTestId('similar-band-counts')).toBeTruthy(), DOCK_WAIT)
    const banner = screen.getByTestId('similar-banner').textContent ?? ''
    expect(banner).toContain('来自：找相似 · 精准匹配')
    expect(banner).toContain('相似度 ≥ 90%')
    expect(screen.getByTestId('similar-band-counts').textContent).toContain('近似重复 1 张 / 同图变体 1 张')
    expect(screen.getByTestId('similar-missing-vectors').textContent).toContain('缺判重向量')

    // 第二轮（同一张图 #1）：从菜单切回语义相似 → 立刻换接口、换结果
    cleanup()
    await renderGallery('/?ids=1')
    await waitMain()
    await openCaretAndPick('semantic')
    await waitFor(
      () => expect(calls.searchSimilar).toHaveBeenCalledWith(1, 60, undefined, undefined, 'semantic'),
      DOCK_WAIT,
    )
    await waitFor(
      () => expect(screen.getByTestId('similar-banner').textContent).toContain('来自：找相似 · 语义相似'),
      DOCK_WAIT,
    )
    expect(screen.queryByTestId('similar-band-counts')).toBeNull()
    expect(_ls.get('dam.similarMode')).toBe('semantic')
  })

  it('旧深链 ?simMode=visual_precise 仍生效（只读兼容）：主键初始即精准档', async () => {
    calls.searchSimilarPrecise.mockResolvedValue({
      ...PRECISE_RESULT,
      hits: [],
      count: 0,
      candidatePool: 0,
      missingDedupVectors: 0,
      queryMissingDedupVector: true,
      dedupIndexReady: false,
    })
    await renderGalleryBrowser('/gallery?ids=1&simMode=visual_precise')
    const main = await waitMain()
    expect(main.textContent).toContain('精准匹配')
    await act(async () => {
      fireEvent.click(main)
    })
    // 精准档但整库没判重向量：显式提示，不报「没有相似图」，也不偷偷退回语义
    await waitFor(() => expect(screen.getByTestId('similar-precise-note')).toBeTruthy(), DOCK_WAIT)
    expect(screen.getByTestId('similar-precise-note').textContent).toContain('判重向量')
    expect(calls.searchSimilar).not.toHaveBeenCalled()
    expect(calls.searchSimilarPrecise).toHaveBeenCalledWith(1, 60)
  })

  it('两档请求体对账（真 api 层）：body.mode = semantic / visual_precise', async () => {
    const actual = await vi.importActual<{ api: typeof import('../src/lib/api').api }>('../src/lib/api')
    const seen: Record<string, unknown>[] = []
    const fetchMock = vi.fn(async (_url: string, init?: RequestInit) => {
      seen.push(JSON.parse(String(init?.body ?? '{}')))
      return new Response('{"model":"m","dims":8,"k":60,"count":0,"hits":[]}', {
        status: 200,
        headers: { 'content-type': 'application/json' },
      })
    })
    const prevFetch = globalThis.fetch
    globalThis.fetch = fetchMock as unknown as typeof fetch
    try {
      await actual.api.searchSimilar(1, 60, undefined, undefined, 'semantic')
      await actual.api.searchSimilarPrecise(1, 60)
    } finally {
      globalThis.fetch = prevFetch
    }
    expect(seen[0]).toMatchObject({ assetId: 1, k: 60, mode: 'semantic' })
    expect(seen[1]).toMatchObject({ assetId: 1, k: 60, mode: 'visual_precise' })
    console.log('[请求体1 语义]', JSON.stringify(seen[0]))
    console.log('[请求体2 精准]', JSON.stringify(seen[1]))
  })
})
