// @vitest-environment jsdom
// @vitest-environment-options {"url": "http://localhost:3000/"}
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter, Route, Routes } from 'react-router-dom'
import { ToastProvider } from '../src/lib/toast'
import { Search } from '../src/pages/Search'
import type { AssetView, SearchHit } from '../src/lib/api'

// jsdom 在本环境拿不到可用的 localStorage（Node 26 的实验性 localStorage 全局也会遮住它），
// 这里给一个最小 Storage 桩：搜索页 → 图库交接临时集合依赖它，测试要能读到真实写入的键值。
const store = new Map<string, string>()
const ls = {
  getItem: (k: string) => store.get(k) ?? null,
  setItem: (k: string, v: string) => void store.set(k, String(v)),
  removeItem: (k: string) => void store.delete(k),
  clear: () => store.clear(),
  key: (i: number) => [...store.keys()][i] ?? null,
  get length() {
    return store.size
  },
} as Storage
Object.defineProperty(globalThis, 'localStorage', { value: ls, configurable: true, writable: true })

/**
 * 卡 t_dc97b141 验收：搜索结果 → 可选卡片 + 选择集 + 批量动作入口。
 *
 * 这里跑的是真组件树（Search 页 + 图库同一张 AssetCard），只把后端 api 换成桩：
 * 命中 3 条 → 渲染 3 张可选卡；点一张 → 选择集 1；点「全选结果」→ 选择集 3，
 * 批量动作条出现且文案写明「作用于全部 3 张」；批量按钮（打分/打标/导出/图库打开）都在。
 */

const HITS: SearchHit[] = [
  { assetId: 101, resourceId: 1, score: 0.991, fileName: 'snow-1.jpg', titleCh: '雪山日出', thumbPath: null, width: 1200, height: 800 },
  { assetId: 102, resourceId: 2, score: 0.973, fileName: 'snow-2.jpg', titleCh: '雪山云海', thumbPath: null, width: 900, height: 1200 },
  { assetId: 103, resourceId: 3, score: 0.955, fileName: 'snow-3.jpg', titleCh: '雪线', thumbPath: null, width: 1000, height: 1000 },
]

function assetOf(id: number): AssetView {
  const base = HITS.find((h) => h.assetId === id)!
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

// vi.mock 工厂会被提升到文件顶部：桩对象必须用 vi.hoisted 先建，实现稍后再挂
const apiStub = vi.hoisted(() => ({
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
  tagCandidates: vi.fn(),
  rate: vi.fn(),
  reject: vi.fn(),
  addTags: vi.fn(),
  removeTag: vi.fn(),
  setColor: vi.fn(),
  moveAssets: vi.fn(),
  embedStart: vi.fn(),
  appInfo: vi.fn(),
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

// 任务中心在搜索页只用来起嵌入任务：整块桩掉，避免测试里拉起全局轮询
vi.mock('../src/lib/tasks', () => ({
  useTasks: () => ({ startEmbed: vi.fn() }),
  TaskCenterProvider: ({ children }: { children: React.ReactNode }) => children,
}))

// 桩实现（HITS 已在上面定义，这里按真实后端形状挂上）
apiStub.searchStatus.mockImplementation(async () => ({
  searchable: true,
  activeModel: 'stub-embed',
  activeDims: 8,
  activeRows: 3,
  otherModelVectors: false,
  emb: { none: 0 },
}))
apiStub.dirs.mockImplementation(async () => ({ dirs: [], roots: [] }))
apiStub.searchText.mockImplementation(async () => ({
  model: 'stub-embed',
  dims: 8,
  k: 24,
  count: HITS.length,
  hits: HITS,
}))
apiStub.searchSimilar.mockImplementation(async () => ({ model: 'stub-embed', dims: 8, k: 24, count: 0, hits: [] }))
apiStub.searchImage.mockImplementation(async () => ({ model: 'stub-embed', dims: 8, k: 24, count: 0, hits: [] }))
apiStub.assets.mockImplementation(async (q: { ids?: number[] }) => ({
  total: (q.ids ?? []).length,
  items: (q.ids ?? []).map(assetOf),
}))
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
apiStub.capabilities.mockImplementation(async () => ({ features: {} }))
apiStub.tagCandidates.mockImplementation(async () => ({ items: [], total: 0, hasMore: false }))
apiStub.rate.mockImplementation(async () => ({ updated: 1 }))
apiStub.reject.mockImplementation(async () => ({ updated: 1 }))
apiStub.addTags.mockImplementation(async (ids: number[]) => ({
  results: ids.map((assetId) => ({ assetId, ok: true })),
  okCount: ids.length,
  failCount: 0,
}))
apiStub.embedStart.mockImplementation(async () => ({ queued: 0, model: 'stub-embed' }))
apiStub.appInfo.mockImplementation(async () => ({ dataDir: '/data' }))

function renderSearch(path = '/search?q=%E9%9B%AA%E5%B1%B1') {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <ToastProvider>
        <Routes>
          <Route path="/search" element={<Search />} />
          <Route path="/gallery" element={<div data-testid="gallery-stub">gallery</div>} />
        </Routes>
      </ToastProvider>
    </MemoryRouter>,
  )
}

describe('搜索页：结果卡片化 + 选择集 + 批量动作（卡 t_dc97b141）', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    store.clear()
  })
  // vitest 未开 globals：RTL 的自动清理不会注册，必须手动 unmount（否则多次 render 累积）
  afterEach(() => cleanup())

  it('命中 3 条 → 渲染 3 张可选卡（与图库同一张 AssetCard），未选时无批量动作条', async () => {
    renderSearch()
    await waitFor(() => expect(screen.getAllByTestId(/^asset-card-/)).toHaveLength(3))
    expect(screen.getByTestId('search-results')).toBeTruthy()
    // 视图顺序 = 命中顺序（相似度降序）
    expect(screen.getAllByTestId(/^asset-card-/).map((el) => el.getAttribute('data-testid'))).toEqual([
      'asset-card-101',
      'asset-card-102',
      'asset-card-103',
    ])
    expect(screen.getByTestId('selection-count').textContent).toContain('已选 0')
    // KI-16 #5：未选时没有批量动作（打标/导出/详情都不渲染），但范围条常驻说明「全选已加载 N 张」
    expect(screen.queryByTestId('batch-tag-open')).toBeNull()
    expect(screen.getByTestId('selection-select-all').textContent).toContain('全选已加载 3 张')
  })

  it('点击卡片 → 选择集 1 张；全选已加载 → 选择集 3 张，批量动作带张数', async () => {
    renderSearch()
    await waitFor(() => expect(screen.getAllByTestId(/^asset-card-/)).toHaveLength(3))

    fireEvent.click(screen.getByTestId('asset-card-101'))
    expect(screen.getByTestId('selection-count').textContent).toContain('已选 1')
    expect(screen.getByTestId('batch-tag-open').textContent).toContain('1 张')
    expect(screen.getByTestId('asset-card-101').getAttribute('data-selected')).toBe('1')

    fireEvent.click(screen.getByTestId('selection-select-all'))
    expect(screen.getByTestId('selection-count').textContent).toContain('已选 3')
    for (const id of [101, 102, 103]) {
      expect(screen.getByTestId(`asset-card-${id}`).getAttribute('data-selected')).toBe('1')
    }

    // 批量动作入口（复用图库既有动作，不另写一套；KI-11：星级按钮已移除）
    expect(screen.queryByTestId('batch-rate-1')).toBeNull()
    expect(screen.queryByTestId('batch-rate-0')).toBeNull()
    expect(screen.getByTestId('batch-tag-open').textContent).toContain('3 张')
    expect(screen.getByTestId('batch-export').textContent).toContain('3 张')
    expect(screen.getByTestId('batch-open-gallery').textContent).toContain('在图库中打开（3 张）')

    fireEvent.click(screen.getByTestId('selection-clear'))
    expect(screen.getByTestId('selection-count').textContent).toContain('已选 0')
    expect(screen.queryByTestId('batch-tag-open')).toBeNull()
  })

  it('批量打标对话框调用 api.addTags；KI-11 后批量条不再有打分入口', async () => {
    renderSearch()
    await waitFor(() => expect(screen.getAllByTestId(/^asset-card-/)).toHaveLength(3))
    fireEvent.click(screen.getByTestId('selection-select-all'))

    expect(screen.queryByTestId('batch-rate-4')).toBeNull()
    expect(document.body.textContent).not.toContain('清除评分')

    fireEvent.click(screen.getByTestId('batch-tag-open'))
    fireEvent.change(screen.getByTestId('batch-tag-input'), { target: { value: '雪山, 日出' } })
    fireEvent.click(screen.getByTestId('batch-tag-submit'))
    await waitFor(() => expect(apiStub.addTags).toHaveBeenCalledWith([101, 102, 103], ['雪山', '日出']))
  })

  it('批量打标：候选选中成 chips（多选），提交时 chips + 输入框文本合并去重', async () => {
    apiStub.tagCandidates.mockImplementation(async () => ({
      items: [
        { value: '尺寸-滚筒_10KG', key: '尺寸', count: 2 },
        { value: '工作-招聘', key: '工作', count: 1 },
      ],
      total: 2,
      hasMore: false,
    }))
    renderSearch()
    await waitFor(() => expect(screen.getAllByTestId(/^asset-card-/)).toHaveLength(3))
    // t_cf4b7daa 后选择条统一成共用 SelectionBar（testid selection-select-all）；
    // 旧 search-select-all 已随旧双条形态退役
    fireEvent.click(screen.getByTestId('selection-select-all'))
    fireEvent.click(screen.getByTestId('batch-tag-open'))

    const input = screen.getByTestId('batch-tag-input')
    fireEvent.focus(input)
    fireEvent.change(input, { target: { value: '尺寸' } })
    // 输入即查（250ms 防抖）→ 候选出现，点选入 chips
    await waitFor(() =>
      expect(screen.getByTestId('batch-tag-input-suggest-item-尺寸-滚筒_10KG')).toBeTruthy(),
    )
    fireEvent.mouseDown(screen.getByTestId('batch-tag-input-suggest-item-尺寸-滚筒_10KG'))
    await waitFor(() => expect(screen.getByTestId('batch-tag-chips').textContent).toContain('尺寸-滚筒_10KG'))
    // 再手输一个新值，提交 = chips + 输入框文本
    fireEvent.change(input, { target: { value: '新标签' } })
    fireEvent.click(screen.getByTestId('batch-tag-submit'))
    await waitFor(() =>
      expect(apiStub.addTags).toHaveBeenCalledWith([101, 102, 103], ['尺寸-滚筒_10KG', '新标签']),
    )
  })

  it('搜索页标签筛选输入 = TagPicker：输入出候选（带计数）→ 点选 → 按完整值走浏览过滤', async () => {
    apiStub.tagCandidates.mockImplementation(async () => ({
      items: [{ value: '工作-招聘', key: '工作', count: 7 }],
      total: 1,
      hasMore: false,
    }))
    // q 为空的纯过滤通道（KI-13 要验证的就是「选中即执行过滤查询」）
    apiStub.assets.mockImplementation(async (q: { tag?: string; ids?: number[] }) => ({
      total: q.tag ? 7 : (q.ids ?? []).length,
      items: [],
    }))
    renderSearch('/search')
    await waitFor(() => expect(apiStub.searchStatus).toHaveBeenCalled())

    const input = screen.getByTestId('tag-filter-input')
    fireEvent.focus(input)
    fireEvent.change(input, { target: { value: '工作' } })
    // 面板立即开（loading 态），候选要等防抖 + 响应 → 先等候选行出现再断言入参
    await waitFor(() => expect(screen.getByTestId('tag-suggest-item-工作-招聘')).toBeTruthy())
    expect(apiStub.tagCandidates).toHaveBeenCalledWith('工作', { limit: 30 })
    const row = screen.getByTestId('tag-suggest-item-工作-招聘')
    expect(row.textContent).toContain('7')

    fireEvent.mouseDown(row)
    expect((screen.getByTestId('tag-filter-input') as HTMLInputElement).value).toBe('工作-招聘')
    // 点选 = 填完整值 → 去抖后按该值走 /api/library/assets?tag=（纯过滤通道）
    await waitFor(
      () =>
        expect(
          apiStub.assets.mock.calls.some(
            (c) => (c[0] as { tag?: string } | undefined)?.tag === '工作-招聘',
          ),
        ).toBe(true),
      { timeout: 3000 },
    )
  })

  it('「在图库中打开」把选择集作为临时集合交接（localStorage）并跳到 /gallery', async () => {
    localStorage.clear()
    renderSearch()
    await waitFor(() => expect(screen.getAllByTestId(/^asset-card-/)).toHaveLength(3))
    fireEvent.click(screen.getByTestId('selection-select-all'))
    fireEvent.click(screen.getByTestId('batch-open-gallery'))

    await waitFor(() => expect(screen.getByTestId('gallery-stub')).toBeTruthy())
    const raw = localStorage.getItem('dam.pendingSet')
    expect(raw).toBeTruthy()
    const pending = JSON.parse(raw!) as { label: string; ids: number[] }
    expect(pending.ids).toEqual([101, 102, 103])
    expect(pending.label).toContain('搜索')
  })

  it('Cmd/Ctrl+A 全选当前结果；Esc 逐层关闭（KI-12：先关检查器抽屉，再清空选择）', async () => {
    renderSearch()
    await waitFor(() => expect(screen.getAllByTestId(/^asset-card-/)).toHaveLength(3))
    fireEvent.keyDown(window, { key: 'a', ctrlKey: true })
    expect(screen.getByTestId('selection-count').textContent).toContain('已选 3')
    // 全选 → 抽屉自动滑出（批量模式）；第一次 Esc 归抽屉
    await waitFor(() => expect(screen.getByTestId('inspector-dock')).toBeTruthy())
    fireEvent.keyDown(window, { key: 'Escape' })
    await waitFor(() => expect(screen.queryByTestId('inspector-dock')).toBeNull())
    expect(screen.getByTestId('selection-count').textContent).toContain('已选 3')
    // 第二次 Esc 才清空选择
    fireEvent.keyDown(window, { key: 'Escape' })
    expect(screen.getByTestId('selection-count').textContent).toContain('已选 0')
  })

  it('KI-12：抽屉手动关掉后可用「详情」按钮重开（搜索侧唯一手动入口）', async () => {
    sessionStorage.clear() // 前面用例 Esc 关过抽屉 → 清掉「本会话不再自动弹」标记
    renderSearch()
    await waitFor(() => expect(screen.getAllByTestId(/^asset-card-/)).toHaveLength(3))
    fireEvent.click(screen.getByTestId('asset-card-101'))
    await waitFor(() => expect(screen.getByTestId('inspector-dock')).toBeTruthy())
    // 手动关掉（本会话不再自动弹）；选择集还在 → 批量条与「详情」按钮仍在
    fireEvent.click(screen.getByTestId('inspector-dock-close'))
    await waitFor(() => expect(screen.queryByTestId('inspector-dock')).toBeNull())
    fireEvent.click(screen.getByTestId('search-inspector-toggle'))
    await waitFor(() => expect(screen.getByTestId('inspector-dock')).toBeTruthy())
    // 再点一次 = 收起
    fireEvent.click(screen.getByTestId('search-inspector-toggle'))
    await waitFor(() => expect(screen.queryByTestId('inspector-dock')).toBeNull())
  })

  it('深链语义不变：?q= 走文搜图、?asset= 走找相似；过滤条件从 URL 恢复（刷新后仍生效）', async () => {
    // 目录/标签：写用 `dir`/`tags`（历史链接与验收脚本都用 dir），`path`/`tag` 作为只读别名继续认
    renderSearch('/search?q=%E9%9B%AA%E5%B1%B1&dir=%2Fphotos&ext=jpg&tags=%E9%A3%8E%E6%99%AF')
    await waitFor(() =>
      expect(apiStub.searchText).toHaveBeenCalledWith('雪山', 24, {
        dirPrefix: '/photos',
        ext: 'jpg',
        tag: '风景',
      }, undefined),
    )
    cleanup()
    vi.clearAllMocks()
    // 冻结契约里的旧写法（path/tag）同样能打开同一次搜索
    renderSearch('/search?q=%E9%9B%AA%E5%B1%B1&path=%2Fphotos&ext=jpg&tag=%E9%A3%8E%E6%99%AF')
    await waitFor(() =>
      expect(apiStub.searchText).toHaveBeenCalledWith('雪山', 24, {
        dirPrefix: '/photos',
        ext: 'jpg',
        tag: '风景',
      }, undefined),
    )
    cleanup()
    vi.clearAllMocks()
    renderSearch('/search?asset=101')
    // KI-14：档位显式透传（mode=semantic），契约从 3 参扩到 5 参 —— 参数顺序不变
    await waitFor(() =>
      expect(apiStub.searchSimilar).toHaveBeenCalledWith(101, 24, expect.anything(), undefined, 'semantic'),
    )
    expect(apiStub.searchText).not.toHaveBeenCalled()
  })
})
