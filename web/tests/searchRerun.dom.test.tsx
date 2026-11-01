// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react'
import { MemoryRouter } from 'react-router-dom'
import { ToastProvider } from '../src/lib/toast'
import { TaskCenterProvider } from '../src/lib/tasks'

/**
 * AUDIT-B2 §1/§2 回归（卡 t_6a771600）：
 * 搜索页此前把过滤条件（扩展名/标签/目录）与条数当摆设——改了不重搜，
 * 上传图路径更是连 k/过滤都不发给后端。这里断言「改 ext → 去抖后自动重搜，且入参带上了新过滤」。
 *
 * 断言的是**真实调用入参**（mock 掉 api 模块，页面逻辑全跑），不是渲染出来的文案。
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

const emptyResult = { model: 'siglip2-base-p224-onnx', dims: 768, k: 24, count: 0, hits: [] }

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
  apiMock.searchText.mockResolvedValue({ ...emptyResult, minScore: 0.085 })
  apiMock.searchSimilar.mockResolvedValue({ ...emptyResult, minScore: 0.72, minScoreCalibrated: true })
  apiMock.searchImage.mockResolvedValue({ ...emptyResult, minScore: 0.72, minScoreCalibrated: true })
  apiMock.assets.mockResolvedValue({ total: 0, items: [] })
  apiMock.tagCandidates.mockResolvedValue({ items: [], total: 0, hasMore: false })
})

afterEach(() => {
  cleanup()
  vi.useRealTimers()
  vi.clearAllMocks()
})

describe('搜索页：过滤条件改了要自动重搜（AUDIT-B2 §2）', () => {
  it('上传图搜索：改扩展名 → 去抖后按同一张图重搜，且 ext 进了 query 入参', async () => {
    await renderSearch()
    await act(async () => {
      await Promise.resolve()
    })

    // 切到「以图搜图」模式，再选一张图上传搜索
    await act(async () => {
      fireEvent.click(screen.getByText('以图搜图'))
    })
    const fileInput = document.querySelector('input[type="file"]') as HTMLInputElement
    expect(fileInput).not.toBeNull()
    const file = new File([new Uint8Array([1, 2, 3])], 'query.png', { type: 'image/png' })
    await act(async () => {
      fireEvent.change(fileInput, { target: { files: [file] } })
      await Promise.resolve()
    })
    expect(apiMock.searchImage).toHaveBeenCalledTimes(1)
    const first = apiMock.searchImage.mock.calls[0]
    expect(first[0]).toBe(file)
    expect(first[1]).toMatchObject({ k: 24 })
    expect(first[1].filter.ext).toBeUndefined()

    // 改扩展名 → 450ms 去抖后自动重搜
    const extInput = screen.getByPlaceholderText('扩展名 jpg')
    await act(async () => {
      fireEvent.change(extInput, { target: { value: 'png' } })
    })
    expect(screen.getByTestId('threshold-bar').textContent).toContain('条件已修改')
    await act(async () => {
      vi.advanceTimersByTime(500)
      await Promise.resolve()
    })

    expect(apiMock.searchImage).toHaveBeenCalledTimes(2)
    const second = apiMock.searchImage.mock.calls[1]
    expect(second[0]).toBe(file) // 同一张查询图
    expect(second[1].filter).toMatchObject({ ext: 'png' })
    expect(screen.getByTestId('threshold-bar').textContent).not.toContain('条件已修改')
  })

  it('文搜图：改标签 → 去抖后带新 tag 重搜；条数变化同样触发', async () => {
    await renderSearch('/search?q=雪山')
    await act(async () => {
      await Promise.resolve()
    })
    expect(apiMock.searchText).toHaveBeenCalledTimes(1)
    expect(apiMock.searchText.mock.calls[0][0]).toBe('雪山')

    const tagInput = screen.getByTestId('tag-filter-input')
    await act(async () => {
      fireEvent.change(tagInput, { target: { value: '风景' } })
      vi.advanceTimersByTime(500)
      await Promise.resolve()
    })
    expect(apiMock.searchText).toHaveBeenCalledTimes(2)
    const second = apiMock.searchText.mock.calls[1]
    expect(second[0]).toBe('雪山')
    expect(second[2]).toMatchObject({ tag: '风景' })

    // 条数改小 → 再重搜一次，k 变化
    const kSelect = screen.getByDisplayValue('24')
    await act(async () => {
      fireEvent.change(kSelect, { target: { value: '12' } })
      vi.advanceTimersByTime(500)
      await Promise.resolve()
    })
    expect(apiMock.searchText).toHaveBeenCalledTimes(3)
    expect(apiMock.searchText.mock.calls[2][1]).toBe(12)
  })

  it('生效阈值在页面上可见（文搜图 0.085 模型标定 / 图搜按模型标定的 72%）', async () => {
    const { unmount } = await renderSearch('/search?q=雪山')
    await act(async () => {
      await Promise.resolve()
    })
    // KI-16 #4：阈值不再是「相似度 ≥ 8.5%」徽标，而是三档控件 + 当前值回显
    expect(screen.getByTestId('simmin-current').textContent).toContain('0.085')
    expect(screen.getByTestId('simmin-current').textContent).toContain('模型标定')
    unmount()

    await renderSearch('/search?asset=7')
    await act(async () => {
      await Promise.resolve()
    })
    expect(screen.getByTestId('threshold-image').textContent).toContain('72%')
  })

  it('空结果能区分「低于阈值」与「库里没有」：文搜图提示切到「宽松」', async () => {
    await renderSearch('/search?q=雪山')
    await act(async () => {
      await Promise.resolve()
    })
    // 默认档下为空 → 文案点明「可能只是低于当前相关性下限」并指向宽松档
    expect(document.body.textContent).toContain('相关性下限')
    expect(document.body.textContent).toContain('切到「宽松」')
    expect(document.body.textContent).toContain('8.5%')

    // 宽松档 = 显式 minScore=0（后端不过滤）——换档走与 ext/k 同一条去抖通道
    await act(async () => {
      fireEvent.click(screen.getByTestId('simmin-loose'))
      await Promise.resolve()
    })
    await act(async () => {
      vi.advanceTimersByTime(500)
      await Promise.resolve()
    })
    const call = apiMock.searchText.mock.calls.at(-1)!
    expect(call[3]).toBe(0)
    expect(screen.getByTestId('simmin-current').textContent).toContain('不过滤')
  })
})
