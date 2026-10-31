// @vitest-environment jsdom
// @vitest-environment-options {"url": "http://localhost:3000/tags"}
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter, Route, Routes } from 'react-router-dom'
import { ToastProvider } from '../src/lib/toast'
import { ConfirmProvider } from '../src/lib/confirm'
import { TagBrowser } from '../src/pages/TagBrowser'
import { LibraryTree, defaultFilter } from '../src/components/LibraryTree'
import { NAV_ITEMS } from '../src/App'
import { __setCapabilitiesForTest } from '../src/lib/caps'
import type { TagCandidateItem, TagGroupItem } from '../src/lib/api'

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

/**
 * task（BATCH-07 B07-4 代码层门禁 / R34）：标签总览 `/tags`。
 *
 * 卡验收里落在前端的行为契约，逐条钉：
 * 1. 左栏 key 分组（值数）+ 裸值组「（无前缀）」；点分组 → 右栏换成该组的值；
 * 2. 右栏值列表：计数 + 样本缩略图 + 复选；点值 → 回图库 `/?tag=<完整值>`（不新开浏览路径）；
 * 3. 分页：`afterCount/afterValue` keyset 续页、上一页弹栈（keyset 不能反算）、页脚页数；
 * 4. 批量合并：确认框数字来自 `merge-preview`（不是前端求和），执行后回执横幅 + 刷新列表；
 * 5. 拒删有资产的值：先给可读原因（点名值 + 张数），不发请求；
 * 6. 入口：主导航**没有** `/tags`（NAV 长度不变），侧栏「标签 → 全部」与命令面板可达。
 *
 * api 模块整体 mock（组件内部逻辑全跑）；TagPicker 复用父卡实现，这里只验证它被接上。
 */

const GROUPS: TagGroupItem[] = [
  { key: '尺寸', values: 3 },
  { key: '颜色', values: 2 },
  { key: '', values: 1 },
]

const ROWS: Record<string, TagCandidateItem[]> = {
  尺寸: [
    { value: '尺寸-滚筒_10KG', key: '尺寸', count: 12 },
    { value: '尺寸-滚筒_5KG', key: '尺寸', count: 3 },
    { value: '尺寸-洗衣机_8KG', key: '尺寸', count: 1 },
  ],
  颜色: [{ value: '颜色-白色', key: '颜色', count: 5 }],
  __keyless: [{ value: 'Sunset', key: null, count: 2 }],
}

const apiMock = vi.hoisted(() => ({
  tagGroups: vi.fn(),
  tagPage: vi.fn(),
  tagSamples: vi.fn(),
  tagMergePreview: vi.fn(),
  tagMerge: vi.fn(),
  tagRename: vi.fn(),
  tagDelete: vi.fn(),
  thumbUrl: (id: number, size: 256 | 1024 = 256) => `/api/media/thumb/${id}?size=${size}`,
}))

vi.mock('../src/lib/api', async (importOriginal) => {
  const actual = await importOriginal<Record<string, unknown>>()
  return { ...actual, api: apiMock }
})

function renderPage(path = '/tags') {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <ToastProvider>
        <ConfirmProvider>
          <Routes>
            <Route path="/tags" element={<TagBrowser />} />
            <Route path="/" element={<div data-testid="gallery-stub">gallery</div>} />
          </Routes>
        </ConfirmProvider>
      </ToastProvider>
    </MemoryRouter>,
  )
}

beforeEach(() => {
  apiMock.tagGroups.mockReset().mockResolvedValue({ items: GROUPS, total: 6 })
  apiMock.tagPage.mockReset().mockImplementation(async (q: { key?: string; keyless?: boolean }) => {
    const items = q.keyless ? ROWS.__keyless : (ROWS[q.key ?? '尺寸'] ?? [])
    return { items, total: items.length, hasMore: false }
  })
  apiMock.tagSamples.mockReset().mockResolvedValue({
    items: [{ value: '尺寸-滚筒_10KG', assetIds: [11, 12] }],
  })
  apiMock.tagMergePreview.mockReset()
  apiMock.tagMerge.mockReset()
  apiMock.tagDelete.mockReset()
})

afterEach(() => {
  cleanup()
})

describe('标签总览：分组 × 值列表 × 分页', () => {
  it('左栏 key 分组（值数）+ 裸值组，点分组 → 右栏换成该组的值', async () => {
    renderPage()
    await waitFor(() => expect(screen.getByTestId('tag-group-尺寸')).toBeTruthy())
    expect(screen.getByTestId('tag-group-尺寸').textContent).toContain('3 值')
    expect(screen.getByTestId('tag-group-plain').textContent).toContain('（无前缀）')

    // 默认第一组（尺寸）
    await waitFor(() => expect(screen.getByTestId('tag-row-尺寸-滚筒_10KG')).toBeTruthy())
    expect(screen.getByTestId('tag-scope-total').textContent).toContain('3 个值')

    // 点「颜色」→ 换组（同一页，不整页刷新）
    await act(async () => {
      fireEvent.click(screen.getByTestId('tag-group-颜色'))
      await Promise.resolve()
    })
    await waitFor(() => expect(apiMock.tagPage).toHaveBeenCalled())
    const last = apiMock.tagPage.mock.calls.at(-1)![0]
    expect(last.key).toBe('颜色')
    expect(last.page).toBeUndefined()
    await waitFor(() => expect(screen.getByTestId('tag-row-颜色-白色')).toBeTruthy())
  })

  it('裸值组走 keyless=1（不是 key=\'\'）', async () => {
    renderPage()
    await waitFor(() => expect(screen.getByTestId('tag-group-plain')).toBeTruthy())
    await act(async () => {
      fireEvent.click(screen.getByTestId('tag-group-plain'))
      await Promise.resolve()
    })
    await waitFor(() => {
      const last = apiMock.tagPage.mock.calls.at(-1)![0]
      expect(last.keyless).toBe(true)
      expect(last.key).toBeUndefined()
    })
    await waitFor(() => expect(screen.getByTestId('tag-row-Sunset')).toBeTruthy())
  })

  it('样本缩略图按值渲染（tagSamples 回来的 assetIds）', async () => {
    renderPage()
    await waitFor(() => expect(screen.getByTestId('tag-sample-尺寸-滚筒_10KG-11')).toBeTruthy())
    const img = screen.getByTestId('tag-sample-尺寸-滚筒_10KG-11') as HTMLImageElement
    expect(img.getAttribute('src')).toBe('/api/media/thumb/11?size=256')
  })

  it('点值 → 跳图库 /?tag=<完整值>', async () => {
    renderPage()
    await waitFor(() => expect(screen.getByTestId('tag-row-尺寸-滚筒_10KG')).toBeTruthy())
    await act(async () => {
      fireEvent.click(screen.getByText('尺寸-滚筒_10KG'))
      await Promise.resolve()
    })
    await waitFor(() => expect(screen.getByTestId('gallery-stub')).toBeTruthy())
  })

  // 单测自身要显式放宽：里面三段 waitFor 各 15s（并行跑 65 个 dom 文件时会被挤占），
  // 但 vitest 默认 testTimeout 也是 5s —— 不放宽这里，waitFor 的超时等于没放宽。
  it('下一页用 keyset 游标（afterCount + afterValue），上一页弹栈不用游标', { timeout: 45000 }, async () => {
    apiMock.tagPage.mockImplementation(async (q: { after?: unknown }) => ({
      items: q.after
        ? [{ value: '尺寸-滚筒_5KG', key: '尺寸', count: 3 }]
        : [{ value: '尺寸-滚筒_10KG', key: '尺寸', count: 12 }],
      total: 3,
      hasMore: !q.after,
    }))
    renderPage()
    // 等「下一页」**可点**再点：按钮 disabled 条件是 `!hasMore || rows.length === 0`，
    // 首次取数在途时 rows 还是空 → 此时点击会被浏览器直接丢弃（mock 只看到 page1 的那次调用，
    // 报 expected null）。负载下这个窗口会被拉长，所以等 enabled 而不是只等元素存在。
    await waitFor(
      () => expect((screen.getByTestId('tag-next') as HTMLButtonElement).disabled).toBe(false),
      { timeout: 15000 },
    )
    await act(async () => {
      fireEvent.click(screen.getByTestId('tag-next'))
      await Promise.resolve()
    })
    await waitFor(() => {
      const last = apiMock.tagPage.mock.calls.at(-1)![0]
      expect(last.after).toEqual({ count: 12, value: '尺寸-滚筒_10KG' })
      // 默认 1s 在并行跑 61 个 dom 用例时不够（点「下一页」→ 状态更新 → mock 调用这条链
      // 会被同进程其它用例挤占，实测在负载下偶发 timeout，报「expected null」而不是逻辑错；
      // 5s 在 task 加了两份 dom 用例后又开始偶发，放宽到 15s —— 断言的是入参不是耗时）
    }, { timeout: 15000 })
    // 上一页 → 回到第 1 页（无游标）
    await act(async () => {
      fireEvent.click(screen.getByTestId('tag-prev'))
      await Promise.resolve()
    })
    await waitFor(() => {
      const last = apiMock.tagPage.mock.calls.at(-1)![0]
      expect(last.after ?? null).toBeNull()
    }, { timeout: 15000 })
    // 用例级超时要留余量：上面三次 waitFor 各自最多 15s，vitest 默认用例超时也是 5s
    // → 门禁并行跑 58 个 dom 文件时「第一步慢」就整条超时（报 Test timed out，不是断言失败）。
  }, 45000)
})

describe('批量合并：预览 → 确认 → 执行 → 回执', () => {
  it('勾两个值 → 合并到…→ 确认框数字来自 merge-preview → 执行后回执并刷新', async () => {
    apiMock.tagMergePreview.mockResolvedValue({
      affected: 15,
      removedValues: ['尺寸-滚筒_5KG'],
      keptValue: '尺寸-滚筒_10KG',
    })
    apiMock.tagMerge.mockResolvedValue({
      affected: 15,
      removedValues: ['尺寸-滚筒_5KG'],
      keptValue: '尺寸-滚筒_10KG',
    })
    renderPage()
    await waitFor(() => expect(screen.getByTestId('tag-check-尺寸-滚筒_5KG')).toBeTruthy())

    // 勾选第二行 → 批量条出现
    await act(async () => {
      fireEvent.click(screen.getByTestId('tag-check-尺寸-滚筒_5KG'))
      await Promise.resolve()
    })
    expect(screen.getByTestId('tag-batch-bar').textContent).toContain('已选 1 项')

    // 打开合并对话框（批量条入口）
    await act(async () => {
      fireEvent.click(screen.getByTestId('tag-batch-merge'))
      await Promise.resolve()
    })
    const target = screen.getByTestId('tag-merge-target')
    await act(async () => {
      fireEvent.change(target, { target: { value: '尺寸-滚筒_10KG' } })
      await Promise.resolve()
    })
    await act(async () => {
      fireEvent.click(screen.getByTestId('tag-merge-confirm'))
      await Promise.resolve()
    })

    // 预览被调用（确认框的数字来源），且执行前没有写库
    await waitFor(() => expect(apiMock.tagMergePreview).toHaveBeenCalledWith(
      ['尺寸-滚筒_5KG'],
      '尺寸-滚筒_10KG',
    ))
    expect(apiMock.tagMerge).not.toHaveBeenCalled()
    // 确认框文案：受影响资产数 + 将消失的值 + 不可撤销
    expect(screen.getByText(/受影响资产：15 张/)).toBeTruthy()
    expect(screen.getByText(/将消失的值：尺寸-滚筒_5KG/)).toBeTruthy()
    expect(screen.getByText(/不可撤销/)).toBeTruthy()

    // 点确认（主按钮 = 「合并 N 张」，UI-UNDO 规范）
    const confirmBtn = screen.getByRole('button', { name: /合并 15 张/ })
    await act(async () => {
      fireEvent.click(confirmBtn)
      await Promise.resolve()
    })
    await waitFor(() => expect(apiMock.tagMerge).toHaveBeenCalledWith(
      ['尺寸-滚筒_5KG'],
      '尺寸-滚筒_10KG',
    ))
    // 回执横幅 + 重新拉列表
    await waitFor(() => expect(screen.getByTestId('tag-receipt').textContent).toContain('影响 15 张'))
    expect(apiMock.tagGroups.mock.calls.length).toBeGreaterThan(1)
  })

  it('目标为空 → 不预览、不执行，提示先选目标', async () => {
    renderPage()
    await waitFor(() => expect(screen.getByTestId('tag-check-尺寸-滚筒_5KG')).toBeTruthy())
    await act(async () => {
      fireEvent.click(screen.getByTestId('tag-check-尺寸-滚筒_5KG'))
      await Promise.resolve()
    })
    await act(async () => {
      fireEvent.click(screen.getByTestId('tag-batch-merge'))
      await Promise.resolve()
    })
    await act(async () => {
      fireEvent.click(screen.getByTestId('tag-merge-confirm'))
      await Promise.resolve()
    })
    expect(apiMock.tagMergePreview).not.toHaveBeenCalled()
    expect(apiMock.tagMerge).not.toHaveBeenCalled()
  })
})

describe('拒删有资产的值（验收 5/7）', () => {
  it('有计数的值点删除 → 给可读原因（点名值 + 张数），不发请求', async () => {
    renderPage()
    await waitFor(() => expect(screen.getByTestId('tag-row-尺寸-滚筒_5KG')).toBeTruthy())
    // 行内「删除」按钮（hover 才显形，但 DOM 里始终在）
    const row = screen.getByTestId('tag-row-尺寸-滚筒_5KG')
    const del = Array.from(row.querySelectorAll('button')).find((b) => b.textContent === '删除')!
    await act(async () => {
      fireEvent.click(del)
      await Promise.resolve()
    })
    expect(apiMock.tagDelete).not.toHaveBeenCalled()
    await waitFor(() => {
      const text = document.body.textContent ?? ''
      expect(text).toContain('这些标签还有资产，不能删除')
      expect(text).toContain('尺寸-滚筒_5KG')
      expect(text).toContain('3 张')
    })
  })
})

describe('入口接线（验收 6）：不进主导航，侧栏/命令面板可达', () => {
  const caps = {
    isDesktop: true,
    storeMode: 'local-disk' as const,
    writableRoots: ['/lib'],
    roots: [{ path: '/lib', writable: true, device: 1 }],
    features: {
      moveAsset: true,
      renameAsset: true,
      relink: true,
      openInOS: true,
      physicalDelete: true,
      copyInImport: true,
      scanExternalPath: true,
    },
    featureReasons: {},
  }

  beforeEach(() => __setCapabilitiesForTest(caps))
  afterEach(() => __setCapabilitiesForTest(null))

  it('主导航项里没有 /tags（KI-35 P1 后 NAV=4：图库/搜索/打标任务/设置）', () => {
    expect(NAV_ITEMS.map((n) => n.to)).not.toContain('/tags')
    expect(NAV_ITEMS.map((n) => n.to)).not.toContain('/export')
    // task：导出收成全局 Dialog，不进主导航；KI-35 P1：能力项动态化，不占静态表
    expect(NAV_ITEMS.length).toBe(4)
  })

  it('图库侧栏「标签」标题行有「全部 →」并指向 /tags', async () => {
    render(
      <MemoryRouter>
        <LibraryTree
          stats={null}
          dirs={[{ dir: '/lib', count: 2 }]}
          roots={['/lib']}
          tags={[{ value: 'Sunset', count: 2 }]}
          filter={defaultFilter}
          dragActive={false}
          onSelect={() => {}}
          onRefreshDirs={() => {}}
          onDropMove={() => {}}
          onDropTag={() => {}}
        />
      </MemoryRouter>,
    )
    const link = screen.getByTestId('sidebar-tags-all')
    expect(link.getAttribute('href')).toBe('/tags')
    expect(link.textContent).toContain('全部')
  })
})
