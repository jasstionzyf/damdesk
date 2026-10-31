// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react'
import { MemoryRouter } from 'react-router-dom'
import { ToastProvider } from '../src/lib/toast'
import { TaskCenterProvider } from '../src/lib/tasks'

/**
 * task（BATCH-07 B07-1/B07-2 代码层门禁）：搜索页「纯过滤浏览」通道 + 标签候选。
 *
 * 真机缺陷：query 为空时搜索按钮灰置（标签/目录/扩展名只能挂在向量搜上，纯过滤无入口）；
 * 标签输入要求精确名，key-val 形态下输入自然词 0 命中且无候选提示。
 *
 * 断言的是**真实调用入参**（mock 掉 api 模块，页面逻辑全跑）：
 * - query 空 + 只填标签 → 按钮可点、走 /api/library/assets（不走向量搜）；
 * - 深链 ?tags=… 首屏自动浏览；
 * - 候选下拉：点选填全值 → 全值进过滤条件；
 * - 无匹配：候选空态「无匹配标签」+ 结果空态「无匹配」；
 * - query 非空行为不变（回归：仍走 searchText）。
 */
// vi.mock 会被提升到文件顶部 → mock 对象必须用 vi.hoisted 先建好
const apiMock = vi.hoisted(() => ({
  searchStatus: vi.fn(),
  dirs: vi.fn(),
  searchText: vi.fn(),
  searchSimilar: vi.fn(),
  searchImage: vi.fn(),
  embedStart: vi.fn(),
  assets: vi.fn(),
  tagCandidates: vi.fn(),
  /** AssetCard 渲染需要（mock 掉整个 api 模块后要自备纯函数） */
  thumbUrl: vi.fn(() => 'blob:thumb'),
  originalUrl: vi.fn(() => 'blob:original'),
}))

vi.mock('../src/lib/api', async (importOriginal) => {
  const actual = await importOriginal<Record<string, unknown>>()
  return { ...actual, api: apiMock }
})

// Search.tsx 里用的是具名导入 { api }，上面已替换；ApiError 需要真实类（instanceof 判断）
async function renderSearch(initial = '/search') {
  const { Search } = await import('../src/pages/Search')
  return render(
    <ToastProvider>
      <TaskCenterProvider>
        <MemoryRouter initialEntries={[initial]}>
          <Search />
        </MemoryRouter>
      </TaskCenterProvider>
    </ToastProvider>,
  )
}

async function settle() {
  await act(async () => {
    await Promise.resolve()
  })
}

const asset = (id: number) => ({
  assetId: id,
  mediaType: 'image',
  absPath: `/lib/p${id}.jpg`,
  fileName: `p${id}.jpg`,
  fileExt: 'jpg',
  thumbPath: null,
  titleCh: null,
  width: 100,
  height: 100,
})

beforeEach(() => {
  vi.useFakeTimers()
  apiMock.searchStatus.mockResolvedValue({
    activeModel: 'siglip2-base-p224-onnx',
    activeDims: 768,
    activeRows: 10,
    searchable: true,
    models: [],
    emb: { none: 0, pending: 0, done: 10, failed: 0 },
    semantic: { total: 10, done: 10, pending: 0, failed: 0 },
    otherModelVectors: false,
    similarMinScore: 0.72,
    similarMinScoreCalibrated: true,
  })
  apiMock.dirs.mockResolvedValue({ dirs: [] })
  apiMock.searchText.mockResolvedValue({
    model: 'siglip2-base-p224-onnx',
    dims: 768,
    k: 24,
    count: 0,
    hits: [],
    minScore: 0.085,
  })
  apiMock.assets.mockResolvedValue({ total: 0, items: [] })
  apiMock.tagCandidates.mockResolvedValue({ items: [], total: 0, hasMore: false })
})

afterEach(() => {
  cleanup()
  vi.useRealTimers()
  vi.clearAllMocks()
})

describe('搜索页纯过滤通道（B07-1）', () => {
  it('query 空 + 只填标签：按钮不灰置，点搜索走 /api/library/assets 且不走向量搜', async () => {
    apiMock.assets.mockResolvedValue({ total: 2, items: [asset(1), asset(2)] })
    await renderSearch()
    await settle()

    const submit = screen.getByTestId('search-submit') as HTMLButtonElement
    expect(submit.disabled).toBe(true) // 无 query 无条件 = 无可搜

    fireEvent.change(screen.getByTestId('tag-filter-input'), { target: { value: '尺寸-滚筒_10KG' } })
    expect(submit.disabled).toBe(false) // B07-1：纯过滤也可搜

    await act(async () => {
      fireEvent.click(submit)
      await Promise.resolve()
    })
    expect(apiMock.assets).toHaveBeenCalledTimes(1)
    const q = apiMock.assets.mock.calls[0][0]
    expect(q.tag).toBe('尺寸-滚筒_10KG')
    expect(q.dirPrefix).toBeUndefined()
    expect(apiMock.searchText).not.toHaveBeenCalled()

    // 结果区：条件浏览徽标 + total（计数对账的唯一真值）+ 同一张卡片网格
    expect(screen.getByTestId('search-source-badge').textContent).toContain('按条件浏览')
    expect(screen.getByTestId('browse-total').textContent).toBe('2')
    expect(screen.getByTestId('search-results')).not.toBeNull()
    expect(screen.getByTestId('search-result-1')).not.toBeNull()
  })

  it('深链 ?tags=… 首屏自动浏览（刷新/分享复现同一次浏览）', async () => {
    apiMock.assets.mockResolvedValue({ total: 1, items: [asset(7)] })
    await renderSearch('/search?tags=%E5%B0%BA%E5%AF%B8-%E6%BB%9A%E7%AD%92_10KG')
    await settle()
    expect(apiMock.assets).toHaveBeenCalledTimes(1)
    expect(apiMock.assets.mock.calls[0][0].tag).toBe('尺寸-滚筒_10KG')
  })

  it('浏览 0 命中 → 空态标题「无匹配」（不是静默空网格）', async () => {
    apiMock.assets.mockResolvedValue({ total: 0, items: [] })
    await renderSearch('/search?tags=%E4%B8%8D%E5%AD%98%E5%9C%A8')
    await settle()
    expect(apiMock.assets).toHaveBeenCalledTimes(1)
    expect(screen.getByText('无匹配')).not.toBeNull()
    expect(document.body.textContent).toContain('没有资产')
  })

  it('query 非空行为不变：点搜索仍走向量搜索（回归护栏）', async () => {
    await renderSearch()
    await settle()
    fireEvent.change(screen.getByPlaceholderText(/中文语义描述/), { target: { value: '雪山' } })
    fireEvent.change(screen.getByTestId('tag-filter-input'), { target: { value: '风景' } })
    await act(async () => {
      fireEvent.click(screen.getByTestId('search-submit'))
      await Promise.resolve()
    })
    expect(apiMock.searchText).toHaveBeenCalledTimes(1)
    expect(apiMock.searchText.mock.calls[0][0]).toBe('雪山')
    expect(apiMock.searchText.mock.calls[0][2]).toMatchObject({ tag: '风景' })
    expect(apiMock.assets).not.toHaveBeenCalled()
  })
})

describe('标签候选下拉（B07-2）', () => {
  it('输入 key 前缀 → 候选按 key 分组展示 + 「选择具体值」提示；点选填全值并触发浏览', async () => {
    apiMock.tagCandidates.mockResolvedValue({
      items: [
        { value: '尺寸-滚筒_10KG', key: '尺寸', count: 2 },
        { value: '尺寸-滚筒_5KG', key: '尺寸', count: 1 },
      ],
      total: 2,
      hasMore: false,
    })
    await renderSearch()
    await settle()

    const input = screen.getByTestId('tag-filter-input')
    fireEvent.focus(input)
    fireEvent.change(input, { target: { value: '尺寸' } })
    await act(async () => {
      vi.advanceTimersByTime(250)
      await Promise.resolve()
    })
    // 候选查询带归一化后的前缀（task：limit 收进 opts，key/offset 同口）
    expect(apiMock.tagCandidates).toHaveBeenCalledWith('尺寸', { limit: 30 })
    expect(screen.getByTestId('tag-suggest')).not.toBeNull()
    expect(screen.getByTestId('tag-suggest-hint').textContent).toContain('选择具体值')
    const item = screen.getByTestId('tag-suggest-item-尺寸-滚筒_10KG')
    expect(item.textContent).toContain('2') // 频次随行

    await act(async () => {
      fireEvent.mouseDown(item)
      await Promise.resolve()
    })
    // 点选 = 填全值（URL 键 tags）→ 去抖后按全值浏览，命中数 = 该值全量
    expect((screen.getByTestId('tag-filter-input') as HTMLInputElement).value).toBe('尺寸-滚筒_10KG')
    await act(async () => {
      vi.advanceTimersByTime(600)
      await Promise.resolve()
    })
    expect(apiMock.assets).toHaveBeenCalledTimes(1)
    expect(apiMock.assets.mock.calls[0][0].tag).toBe('尺寸-滚筒_10KG')
  })

  it('输入不存在的值 → 候选空态「无匹配标签」（不静默）', async () => {
    await renderSearch()
    await settle()
    const input = screen.getByTestId('tag-filter-input')
    fireEvent.focus(input)
    fireEvent.change(input, { target: { value: '不存在' } })
    await act(async () => {
      vi.advanceTimersByTime(250)
      await Promise.resolve()
    })
    expect(screen.getByTestId('tag-suggest-empty').textContent).toBe('无匹配标签')
  })
})
