// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter, Route, Routes, useLocation } from 'react-router-dom'
import { ConfirmProvider } from '../src/lib/confirm'
import { ToastProvider } from '../src/lib/toast'
import { Dedup } from '../src/pages/Dedup'
import { Search } from '../src/pages/Search'
import type { AssetView, DupGroupView, DupMemberView, SearchHit } from '../src/lib/api'

// i18n-C（卡 t_06629257）：本文件断言的是**中文界面原文**，而迁移后文案走 t()、
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
 * KI-16 #4/#5/#6（卡 t_cf4b7daa）DOM 验收：参数控件「人话化」。
 *
 *  #4 搜索页相关性下限：三档（严格 0.1275 / 默认不传 / 宽松 0）+ 当前值回显 + URL `?simMin=` 单一真源；
 *     「放宽阈值」按钮（loosen-threshold）彻底消失。
 *  #5 批量/选择条（共享 SelectionBar）：文案「全选已加载 N 张 / 清空选择」，N = 网格卡片数；
 *     命中总数可知且大于已加载时出提示行「仅作用于已加载的 N 张（共 M 张）」。
 *  #6 去重页：范围收敛为一个下拉（dedup-range-select），「只看已拒选」为可点选/可取消 chip，
 *     删除相关动作统一叫「移入回收站」。
 *
 * 断言的是**真实调用入参**与**真实 DOM**（api 换桩，页面逻辑全跑）。
 */

// ---------- 搜索页桩数据 ----------
const HITS: SearchHit[] = [
  { assetId: 101, resourceId: 1, score: 0.991, fileName: 'snow-1.jpg', titleCh: '雪山日出', thumbPath: null, width: 1200, height: 800 },
  { assetId: 102, resourceId: 2, score: 0.973, fileName: 'snow-2.jpg', titleCh: '雪山云海', thumbPath: null, width: 900, height: 1200 },
  { assetId: 103, resourceId: 3, score: 0.955, fileName: 'snow-3.jpg', titleCh: '雪线', thumbPath: null, width: 1000, height: 1000 },
]

function assetOf(id: number): AssetView {
  const base = HITS.find((h) => h.assetId === id) ?? HITS[0]
  return {
    assetId: id,
    mediaType: 'image',
    absPath: `/library/${base.fileName}`,
    storeMode: 'index',
    missing: 0,
    fileName: base.fileName,
    fileExt: 'jpg',
    fileSz: 2048,
    mtime: 0,
    fileHash: null,
    titleCh: base.titleCh,
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
    width: base.width,
    height: base.height,
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

// ---------- 去重页桩数据 ----------
function member(assetId: number, rejected: boolean): DupMemberView {
  return {
    assetId,
    fileName: `dup-${assetId}.jpg`,
    absPath: `/library/dup-${assetId}.jpg`,
    fileSz: 4096,
    importedAt: 0,
    fileHash: `hash-${assetId}`,
    pHash: `phash-${assetId}`,
    width: 800,
    height: 600,
    starRating: 0,
    rejected,
    similarity: assetId === 1 ? 1 : 0.97,
  }
}

// 组 1 有已拒选成员，组 2 没有 → 「只看已拒选」应把 2 组筛成 1 组
const GROUPS: DupGroupView[] = [
  { groupId: 1, kind: 'near', size: 2, suggestedKeep: 1, reclaimBytes: 4096, members: [member(11, false), member(12, true)] },
  { groupId: 2, kind: 'near', size: 2, suggestedKeep: 21, reclaimBytes: 4096, members: [member(21, false), member(22, false)] },
]

const apiStub = vi.hoisted(() => ({
  // 搜索
  searchStatus: vi.fn(),
  dirs: vi.fn(),
  searchText: vi.fn(),
  searchSimilar: vi.fn(),
  searchImage: vi.fn(),
  assets: vi.fn(),
  capabilities: vi.fn(),
  thumbUrl: (id: number, size: 256 | 1024 = 256) => `/api/media/thumb/${id}?size=${size}`,
  originalUrl: (id: number) => `/api/media/original/${id}`,
  assetTags: vi.fn(),
  assetTagResults: vi.fn(),
  ocrAssetText: vi.fn(),
  complianceVerdicts: vi.fn(),
  complianceReview: vi.fn(),
  rate: vi.fn(),
  reject: vi.fn(),
  addTags: vi.fn(),
  removeTag: vi.fn(),
  setColor: vi.fn(),
  moveAssets: vi.fn(),
  embedStart: vi.fn(),
  appInfo: vi.fn(),
  // 去重
  dupGroups: vi.fn(),
  dedupTrash: vi.fn(),
  dedupResolve: vi.fn(),
  dedupIgnore: vi.fn(),
  dedupUnignore: vi.fn(),
  dedupExportUrl: vi.fn(() => '/api/dedup/export?format=csv&range=all'),
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

// 任务中心在搜索页只用来起嵌入任务：整块桩掉，避免拉起全局轮询
vi.mock('../src/lib/tasks', () => ({
  useTasks: () => ({ startEmbed: vi.fn() }),
  TaskCenterProvider: ({ children }: { children: React.ReactNode }) => children,
}))

/** 记录当前 router 的 search string（断言 URL 单一真源 / 刷新可复现） */
let currentSearch = ''
function UrlProbe() {
  currentSearch = useLocation().search
  return null
}

function renderSearch(path = '/search?q=%E9%9B%AA%E5%B1%B1') {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <UrlProbe />
      <ToastProvider>
        <Routes>
          <Route path="/search" element={<Search />} />
          <Route path="/gallery" element={<div data-testid="gallery-stub">gallery</div>} />
        </Routes>
      </ToastProvider>
    </MemoryRouter>,
  )
}

function renderDedup(path = '/dedup') {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <UrlProbe />
      <ToastProvider>
        <ConfirmProvider>
          <Routes>
            <Route path="/dedup" element={<Dedup />} />
            <Route path="/trash" element={<div data-testid="trash-stub">trash</div>} />
          </Routes>
        </ConfirmProvider>
      </ToastProvider>
    </MemoryRouter>,
  )
}

beforeEach(() => {
  vi.clearAllMocks()
  currentSearch = ''
  // 搜索
  apiStub.capabilities.mockImplementation(async () => ({ features: {} }))
  apiStub.searchStatus.mockImplementation(async () => ({
    searchable: true,
    activeModel: 'stub-embed',
    activeDims: 8,
    activeRows: 3,
    otherModelVectors: false,
    emb: { none: 0 },
    similarMinScore: 0.72,
    similarMinScoreCalibrated: true,
  }))
  apiStub.dirs.mockImplementation(async () => ({ dirs: [], roots: [] }))
  apiStub.searchText.mockImplementation(async () => ({
    model: 'stub-embed',
    dims: 8,
    k: 24,
    count: HITS.length,
    hits: HITS,
    minScore: 0.085,
  }))
  apiStub.searchSimilar.mockImplementation(async () => ({ model: 'stub-embed', dims: 8, k: 24, count: 0, hits: [] }))
  apiStub.searchImage.mockImplementation(async () => ({ model: 'stub-embed', dims: 8, k: 24, count: 0, hits: [] }))
  // 纯过滤浏览：命中总数远大于本次加载（用来验「仅作用于已加载」提示行）
  apiStub.assets.mockImplementation(async () => ({ total: 1200, items: HITS.map((h) => assetOf(h.assetId)) }))
  apiStub.assetTags.mockImplementation(async () => [])
  apiStub.assetTagResults.mockImplementation(async () => [])
  apiStub.ocrAssetText.mockImplementation(async (assetId: number) => ({ assetId, status: 'none', text: null, blocks: [] }))
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
  apiStub.complianceReview.mockImplementation(async () => ({ items: [], total: 0, nextCursor: null }))
  apiStub.addTags.mockImplementation(async (ids: number[]) => ({
    results: ids.map((assetId) => ({ assetId, ok: true })),
    okCount: ids.length,
    failCount: 0,
  }))
  apiStub.embedStart.mockImplementation(async () => ({ queued: 0, model: 'stub-embed' }))
  apiStub.appInfo.mockImplementation(async () => ({ dataDir: '/data' }))
  // 去重
  apiStub.dupGroups.mockImplementation(async () => ({
    groups: GROUPS,
    judge: { model: 'dinov3-vits16-onnx', vectors: 100, edges: 5, backfillDone: true, backfillTruncated: false, lastJudgeAt: 0 },
    ignoredCount: 0,
  }))
  apiStub.dedupTrash.mockImplementation(async () => ({ items: [] }))
})

afterEach(() => {
  cleanup()
  vi.useRealTimers()
})

describe('KI-16 #4 搜索页：相关性下限三档（参数控件人话化）', () => {
  it('「放宽阈值」按钮已消失，控件是 严格/默认/宽松 三档 + 当前值回显', async () => {
    renderSearch()
    await waitFor(() => expect(screen.getAllByTestId(/^asset-card-/)).toHaveLength(3))
    expect(screen.queryByTestId('loosen-threshold')).toBeNull()
    for (const lv of ['strict', 'default', 'loose']) {
      expect(screen.getByTestId(`simmin-${lv}`)).toBeTruthy()
    }
    expect(screen.getByTestId('simmin-current').textContent).toContain('0.085（模型标定）')
    expect(screen.getByTestId('simmin-default').getAttribute('aria-pressed')).toBe('true')
  })

  it('三档切换改的是请求里的 minScore：严格 0.1275 / 默认 不传 / 宽松 0，且写进 URL', async () => {
    vi.useFakeTimers()
    renderSearch()
    await act(async () => {
      await Promise.resolve()
    })
    // 首屏（默认档）不显式传 minScore → 后端按模型标定兜底
    expect(apiStub.searchText).toHaveBeenCalledTimes(1)
    expect(apiStub.searchText.mock.calls[0][3]).toBeUndefined()

    // 严格 = 默认 × 1.5 = 0.1275
    await act(async () => {
      fireEvent.click(screen.getByTestId('simmin-strict'))
      await Promise.resolve()
    })
    await act(async () => {
      vi.advanceTimersByTime(500)
      await Promise.resolve()
    })
    expect(apiStub.searchText.mock.calls.at(-1)![3]).toBe(0.1275)
    expect(currentSearch).toContain('simMin=0.1275')
    expect(screen.getByTestId('simmin-current').textContent).toContain('0.1275')

    // 默认 = 不传（URL 键被清掉）
    await act(async () => {
      fireEvent.click(screen.getByTestId('simmin-default'))
      await Promise.resolve()
    })
    await act(async () => {
      vi.advanceTimersByTime(500)
      await Promise.resolve()
    })
    expect(apiStub.searchText.mock.calls.at(-1)![3]).toBeUndefined()
    expect(currentSearch).not.toContain('simMin')

    // 宽松 = 显式 0（后端不过滤）
    await act(async () => {
      fireEvent.click(screen.getByTestId('simmin-loose'))
      await Promise.resolve()
    })
    await act(async () => {
      vi.advanceTimersByTime(500)
      await Promise.resolve()
    })
    expect(apiStub.searchText.mock.calls.at(-1)![3]).toBe(0)
    expect(currentSearch).toContain('simMin=0')
    expect(screen.getByTestId('simmin-loose').getAttribute('aria-pressed')).toBe('true')
    expect(screen.getByTestId('simmin-current').textContent).toContain('不过滤')
  })

  it('深链 ?simMin=0 打开即「宽松」，刷新（重新挂载同一 URL）后仍保持', async () => {
    const first = renderSearch('/search?q=%E9%9B%AA%E5%B1%B1&simMin=0')
    await waitFor(() => expect(apiStub.searchText).toHaveBeenCalled())
    expect(screen.getByTestId('simmin-loose').getAttribute('aria-pressed')).toBe('true')
    expect(screen.getByTestId('simmin-current').textContent).toContain('0（不过滤）')
    // 首屏就按 URL 里的 0 发请求（不是先发默认再改）
    expect(apiStub.searchText.mock.calls[0][3]).toBe(0)
    const afterMount = currentSearch
    first.unmount()

    // 「刷新」= 用同一 URL 重新挂载：控件回显与请求入参都必须一致
    apiStub.searchText.mockClear()
    renderSearch(`/search${afterMount}`)
    await waitFor(() => expect(apiStub.searchText).toHaveBeenCalled())
    expect(apiStub.searchText.mock.calls[0][3]).toBe(0)
    expect(screen.getByTestId('simmin-loose').getAttribute('aria-pressed')).toBe('true')
    expect(currentSearch).toContain('simMin=0')
  })

  it('高级：手动填数值 → 自定义档，请求带该值且 URL 记录它', async () => {
    vi.useFakeTimers()
    renderSearch()
    await act(async () => {
      await Promise.resolve()
    })
    await act(async () => {
      fireEvent.change(screen.getByTestId('simmin-manual'), { target: { value: '0.2' } })
      await Promise.resolve()
    })
    await act(async () => {
      vi.advanceTimersByTime(500)
      await Promise.resolve()
    })
    expect(apiStub.searchText.mock.calls.at(-1)![3]).toBe(0.2)
    expect(currentSearch).toContain('simMin=0.2')
    expect(screen.getByTestId('simmin-current').textContent).toContain('手动')
  })
})

describe('KI-16 #5 搜索页：批量条文案说清范围（共享 SelectionBar）', () => {
  it('「全选已加载 N 张」的 N 与网格卡片数一致；命中总数更大时给提示行', async () => {
    // 纯过滤浏览：加载 3 张，总数 1200 → 必须明说「仅作用于已加载的 3 张（共 1200 张）」
    renderSearch('/search?ext=jpg')
    await waitFor(() => expect(screen.getAllByTestId(/^asset-card-/)).toHaveLength(3))
    expect(screen.queryByTestId('batch-scope')).toBeNull() // 笼统的「范围/作用于全部」文案已删
    expect(screen.getByTestId('selection-select-all').textContent).toContain('全选已加载 3 张')
    expect(screen.getByTestId('selection-clear').textContent).toContain('清空选择')
    expect(screen.getByTestId('selection-hint').textContent).toContain('仅作用于已加载的 3 张（共 1200 张）')

    // 全选后批量动作只作用于这 3 张（已选 3）
    fireEvent.click(screen.getByTestId('selection-select-all'))
    expect(screen.getByTestId('selection-count').textContent).toContain('已选 3')
    expect(screen.getByTestId('batch-export').textContent).toContain('3 张')
  })

  it('文搜图（总数不可知）不出提示行，但全选文案仍写明「已加载」', async () => {
    renderSearch()
    await waitFor(() => expect(screen.getAllByTestId(/^asset-card-/)).toHaveLength(3))
    expect(screen.getByTestId('selection-select-all').textContent).toContain('全选已加载 3 张')
    expect(screen.queryByTestId('selection-hint')).toBeNull()
  })
})

describe('KI-16 #6 去重页：范围一个下拉 + 只看已拒选 chip + 动作口径', () => {
  it('范围收敛为一个 select：range-all / range-current / range-clear 消失', async () => {
    renderDedup()
    await waitFor(() => expect(screen.getByTestId('dedup-group-1')).toBeTruthy())
    const sel = screen.getByTestId('dedup-range-select') as HTMLSelectElement
    expect(sel.value).toBe('all')
    expect(screen.getByText('全库')).toBeTruthy()
    expect(screen.queryByTestId('range-all')).toBeNull()
    expect(screen.queryByTestId('range-current')).toBeNull()
    expect(screen.queryByTestId('range-clear')).toBeNull()
  })

  it('深链 ?range=dir: 时下拉回显「当前筛选（条件）」，切回全库即清 URL 范围', async () => {
    renderDedup('/dedup?range=dir%3A%2Fphotos%2F2024')
    await waitFor(() => expect(screen.getByTestId('dedup-group-1')).toBeTruthy())
    const sel = screen.getByTestId('dedup-range-select') as HTMLSelectElement
    expect(sel.value).toBe('dir:/photos/2024')
    const opt = Array.from(sel.options).find((o) => o.value === 'dir:/photos/2024')!
    expect(opt.textContent).toContain('当前筛选（文件夹 /photos/2024）')

    await act(async () => {
      fireEvent.change(sel, { target: { value: 'all' } })
      await Promise.resolve()
    })
    expect(currentSearch).not.toContain('range=')
  })

  it('「只看已拒选」可点选与取消：组数 2 → 1 → 2', async () => {
    renderDedup()
    await waitFor(() => expect(screen.getByTestId('dedup-group-1')).toBeTruthy())
    const count = () => screen.queryAllByTestId(/^dedup-group-/).length
    expect(count()).toBe(2)
    expect(screen.getByTestId('rejected-filter').textContent).toContain('只看已拒选')

    fireEvent.click(screen.getByTestId('rejected-filter'))
    expect(count()).toBe(1)
    expect(screen.queryByTestId('dedup-group-1')).toBeTruthy()
    expect(screen.queryByTestId('dedup-group-2')).toBeNull()
    expect(screen.getByTestId('rejected-filter').getAttribute('aria-pressed')).toBe('true')

    fireEvent.click(screen.getByTestId('rejected-filter'))
    expect(count()).toBe(2)
    expect(screen.getByTestId('rejected-filter').getAttribute('aria-pressed')).toBe('false')
  })

  it('回收站入口文案统一为「移入回收站」（不再出现「打开回收站」）', async () => {
    renderDedup()
    await waitFor(() => expect(screen.getByTestId('go-trash')).toBeTruthy())
    expect(screen.getByTestId('go-trash').textContent).toContain('移入回收站')
    expect(document.body.textContent).not.toContain('打开回收站')
  })
})
