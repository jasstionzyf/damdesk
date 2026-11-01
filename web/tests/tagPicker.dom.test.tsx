// @vitest-environment jsdom
// @vitest-environment-options {"url": "http://localhost:3000/"}
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { useState } from 'react'
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react'
import { MemoryRouter } from 'react-router-dom'
import { TagPicker } from '../src/components/TagPicker'
import { LibraryTree, defaultFilter } from '../src/components/LibraryTree'
import { __setCapabilitiesForTest } from '../src/lib/caps'
import type { TagCandidateItem } from '../src/lib/api'

/**
 * t_d4895738（BATCH-07 B07-8 代码层门禁 / R40 / KI-13）：统一标签输入 `TagPicker`。
 *
 * 真机缺陷（KI-13）：图库侧栏 / 检查器 / 批量打标都是裸 input，候选逻辑只在搜索页自写一份；
 * 侧栏既没有筛选入口，也没有「输入前缀出候选、选中即过滤」的能力。
 *
 * 这里钉的是组件的**行为契约**（mock 掉 api 模块，组件内部逻辑全跑）：
 * 1. 输入即查：≥1 字符触发，250ms 防抖，前缀先归一化（首尾空格/内部空白 → `_`）；
 * 2. 候选按 key 分组展示 + 频次随行（服务端 key 字段；裸值进「（未分组）」）；
 * 3. 键盘可达：↑↓ 移动高亮（循环）、Enter 确认、Esc 关闭；
 * 4. 场景区分：筛选场景无匹配只给「无匹配标签」不新建；打标场景提示「回车将新建」并回调原文；
 * 5. 侧栏接线：选中候选 = 执行过滤（tag 进 LibraryFilter，由页面落到 URL）；
 * 6. 只输入 key 时提示「请选择具体值」（筛选场景）。
 */

const apiMock = vi.hoisted(() => ({
  tagCandidates: vi.fn(),
}))

vi.mock('../src/lib/api', async (importOriginal) => {
  const actual = await importOriginal<Record<string, unknown>>()
  return { ...actual, api: apiMock }
})

const CANDIDATES: TagCandidateItem[] = [
  { value: '尺寸-滚筒_10KG', key: '尺寸', count: 12 },
  { value: '尺寸-滚筒_5KG', key: '尺寸', count: 3 },
  { value: '工作-招聘', key: '工作', count: 7 },
  { value: '青岛', key: null, count: 2 },
]

beforeEach(() => {
  vi.useFakeTimers()
  apiMock.tagCandidates.mockReset()
  apiMock.tagCandidates.mockResolvedValue({ items: CANDIDATES, total: CANDIDATES.length, hasMore: false })
})

afterEach(() => {
  vi.useRealTimers()
  cleanup()
})

/** 受控包装：让 value/onChange 真的生效（组件本身是受控的） */
function Harness(props: {
  scenario?: 'filter' | 'tag'
  suggestTestid?: string
  onPick?: (v: string) => void
  onCommit?: (raw: string) => void
}) {
  const [value, setValue] = useState('')
  return (
    <TagPicker
      testid="tp"
      suggestTestid={props.suggestTestid}
      value={value}
      onChange={setValue}
      onPick={(v) => {
        props.onPick?.(v)
        setValue(v)
      }}
      onCommit={props.onCommit}
      scenario={props.scenario ?? 'filter'}
      placeholder="输入标签"
      className="w-40"
    />
  )
}

async function type(text: string) {
  const input = screen.getByTestId('tp')
  fireEvent.focus(input)
  fireEvent.change(input, { target: { value: text } })
  await act(async () => {
    vi.advanceTimersByTime(250)
    await Promise.resolve()
  })
}

describe('TagPicker：输入即查 + 防抖', () => {
  it('≥1 字符触发、250ms 防抖；前缀先归一化（首尾空格 + 内部空白 → _）', async () => {
    render(<Harness />)
    const input = screen.getByTestId('tp')
    fireEvent.focus(input)
    fireEvent.change(input, { target: { value: '滚 筒' } })
    // 防抖窗口内不查（快速敲字只发最后一发）
    await act(async () => {
      vi.advanceTimersByTime(200)
      await Promise.resolve()
    })
    expect(apiMock.tagCandidates).not.toHaveBeenCalled()
    await act(async () => {
      vi.advanceTimersByTime(60)
      await Promise.resolve()
    })
    expect(apiMock.tagCandidates).toHaveBeenCalledTimes(1)
    expect(apiMock.tagCandidates).toHaveBeenCalledWith('滚_筒', { limit: 30 })
  })

  it('清空输入 → 面板收起、不再查', async () => {
    render(<Harness />)
    await type('尺寸')
    expect(screen.getByTestId('tp-suggest')).toBeTruthy()
    await type('')
    expect(screen.queryByTestId('tp-suggest')).toBeNull()
    expect(apiMock.tagCandidates).toHaveBeenCalledTimes(1)
  })
})

describe('TagPicker：候选分组 + 频次', () => {
  it('按 key 分组（服务端 key 字段），频次随行；裸值进「（未分组）」', async () => {
    render(<Harness />)
    await type('尺')
    expect(screen.getByTestId('tp-suggest')).toBeTruthy()
    expect(screen.getByTestId('tp-suggest-group-尺寸')).toBeTruthy()
    expect(screen.getByTestId('tp-suggest-group-工作')).toBeTruthy()
    expect(screen.getByTestId('tp-suggest-group-（未分组）')).toBeTruthy()
    const row = screen.getByTestId('tp-suggest-item-尺寸-滚筒_10KG')
    expect(row.textContent).toContain('尺寸-滚筒_10KG')
    expect(row.textContent).toContain('12')
  })

  it('只输入 key（无 `-`）且确有候选 → 提示「请选择具体值」', async () => {
    render(<Harness />)
    await type('尺寸')
    expect(screen.getByTestId('tp-suggest-hint').textContent).toContain('请选择具体值')
  })

  it('打标场景不给「只输入了 key」提示（提示只服务筛选）', async () => {
    render(<Harness scenario="tag" onCommit={() => {}} />)
    await type('尺寸')
    expect(screen.queryByTestId('tp-suggest-hint')).toBeNull()
  })
})

describe('TagPicker：键盘（↑↓ / Enter / Esc）', () => {
  it('↓ 高亮第一项、再 ↓ 第二项，Enter 选中高亮项（完整值）', async () => {
    const picked: string[] = []
    render(<Harness onPick={(v) => picked.push(v)} />)
    await type('尺寸')
    const input = screen.getByTestId('tp')
    fireEvent.keyDown(input, { key: 'ArrowDown' })
    expect(screen.getByTestId('tp-suggest-item-尺寸-滚筒_10KG').getAttribute('aria-selected')).toBe('true')
    fireEvent.keyDown(input, { key: 'ArrowDown' })
    expect(screen.getByTestId('tp-suggest-item-尺寸-滚筒_5KG').getAttribute('aria-selected')).toBe('true')
    await act(async () => {
      fireEvent.keyDown(input, { key: 'Enter' })
      await Promise.resolve()
    })
    expect(picked).toEqual(['尺寸-滚筒_5KG'])
    expect(screen.queryByTestId('tp-suggest')).toBeNull()
  })

  it('↑ 从第一项回绕到最后一项', async () => {
    render(<Harness />)
    await type('尺寸')
    const input = screen.getByTestId('tp')
    fireEvent.keyDown(input, { key: 'ArrowUp' })
    const last = CANDIDATES[CANDIDATES.length - 1].value
    expect(screen.getByTestId(`tp-suggest-item-${last}`).getAttribute('aria-selected')).toBe('true')
  })

  it('Esc 收起面板且不选中（也不把 Esc 冒泡出去）', async () => {
    const picked: string[] = []
    const outer = vi.fn()
    window.addEventListener('keydown', outer)
    render(<Harness onPick={(v) => picked.push(v)} />)
    await type('尺寸')
    const input = screen.getByTestId('tp')
    fireEvent.keyDown(input, { key: 'Escape' })
    expect(screen.queryByTestId('tp-suggest')).toBeNull()
    expect(picked).toEqual([])
    expect(outer).not.toHaveBeenCalled()
    window.removeEventListener('keydown', outer)
  })

  it('点选候选（mousedown）→ onPick(完整值)', async () => {
    const picked: string[] = []
    render(<Harness onPick={(v) => picked.push(v)} />)
    await type('工作')
    await act(async () => {
      fireEvent.mouseDown(screen.getByTestId('tp-suggest-item-工作-招聘'))
      await Promise.resolve()
    })
    expect(picked).toEqual(['工作-招聘'])
  })
})

describe('TagPicker：无匹配的场景区分（R35 不许筛选场景新建）', () => {
  it('筛选场景无匹配 → 「无匹配标签」，Enter 不新建（onCommit 不回调）', async () => {
    apiMock.tagCandidates.mockResolvedValue({ items: [], total: 0, hasMore: false })
    const picked: string[] = []
    const committed: string[] = []
    render(<Harness scenario="filter" onPick={(v) => picked.push(v)} onCommit={(v) => committed.push(v)} />)
    await type('不存在的标签')
    expect(screen.getByTestId('tp-suggest-empty').textContent).toContain('无匹配标签')
    const input = screen.getByTestId('tp')
    await act(async () => {
      fireEvent.keyDown(input, { key: 'Enter' })
      await Promise.resolve()
    })
    expect(picked).toEqual([])
    expect(committed).toEqual([])
  })

  it('打标场景无匹配 → 提示「回车将新建「…」」，Enter 回调原文（多词已归一化由调用方处理）', async () => {
    apiMock.tagCandidates.mockResolvedValue({ items: [], total: 0, hasMore: false })
    const committed: string[] = []
    render(<Harness scenario="tag" onCommit={(v) => committed.push(v)} />)
    await type('新 标签, 另一个')
    expect(screen.getByTestId('tp-suggest-create').textContent).toContain('回车将新建')
    expect(screen.getByTestId('tp-suggest-create').textContent).toContain('新 标签, 另一个')
    const input = screen.getByTestId('tp')
    await act(async () => {
      fireEvent.keyDown(input, { key: 'Enter' })
      await Promise.resolve()
    })
    // 传原文（逗号列表不能被整体归一化成 `新_标签,_另一个`）
    expect(committed).toEqual(['新 标签, 另一个'])
  })

  it('打标场景有匹配时 Enter 走选中而不是新建', async () => {
    const picked: string[] = []
    const committed: string[] = []
    render(<Harness scenario="tag" onPick={(v) => picked.push(v)} onCommit={(v) => committed.push(v)} />)
    await type('尺寸')
    const input = screen.getByTestId('tp')
    fireEvent.keyDown(input, { key: 'ArrowDown' })
    await act(async () => {
      fireEvent.keyDown(input, { key: 'Enter' })
      await Promise.resolve()
    })
    expect(picked).toEqual(['尺寸-滚筒_10KG'])
    expect(committed).toEqual([])
  })
})

describe('KI-13 侧栏接线：图库侧栏标签输入 → 候选 → 选中即过滤', () => {
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

  it('输入 工作 → 候选（带计数）→ 点选 → onSelect 带 filter.tag（页面据此写 URL、网格据此过滤）', async () => {
    const selected: unknown[] = []
    render(
      <MemoryRouter>
        <LibraryTree
          stats={null}
          dirs={[{ dir: '/lib', count: 2 }]}
          roots={['/lib']}
          tags={[{ value: '尺寸-滚筒_10KG', count: 12 }]}
          filter={defaultFilter}
          dragActive={false}
          onSelect={(f) => selected.push(f)}
          onRefreshDirs={() => {}}
          onDropMove={() => {}}
          onDropTag={() => {}}
        />
      </MemoryRouter>,
    )
    const input = screen.getByTestId('sidebar-tag-input')
    fireEvent.focus(input)
    fireEvent.change(input, { target: { value: '工作' } })
    await act(async () => {
      vi.advanceTimersByTime(250)
      await Promise.resolve()
    })
    expect(apiMock.tagCandidates).toHaveBeenCalledWith('工作', { limit: 30 })
    const row = screen.getByTestId('sidebar-tag-input-suggest-item-工作-招聘')
    expect(row.textContent).toContain('7') // 计数随行（KI-13 要求带计数）
    await act(async () => {
      fireEvent.mouseDown(row)
      await Promise.resolve()
    })
    expect(selected).toEqual([{ view: 'all', untagged: undefined, tag: '工作-招聘' }])
    // 选中后清空草稿，面板收起（避免挡住下面的标签云）
    expect((screen.getByTestId('sidebar-tag-input') as HTMLInputElement).value).toBe('')
    expect(screen.queryByTestId('sidebar-tag-input-suggest')).toBeNull()
  })

  it('侧栏输入不存在的值 → 「无匹配标签」（不静默、不新建）', async () => {
    apiMock.tagCandidates.mockResolvedValue({ items: [], total: 0, hasMore: false })
    render(
      <MemoryRouter>
        <LibraryTree
          stats={null}
          dirs={[]}
          roots={['/lib']}
          tags={[]}
          filter={defaultFilter}
          dragActive={false}
          onSelect={() => {}}
          onRefreshDirs={() => {}}
          onDropMove={() => {}}
          onDropTag={() => {}}
        />
      </MemoryRouter>,
    )
    const input = screen.getByTestId('sidebar-tag-input')
    fireEvent.focus(input)
    fireEvent.change(input, { target: { value: '不存在' } })
    await act(async () => {
      vi.advanceTimersByTime(250)
      await Promise.resolve()
    })
    expect(screen.getByTestId('sidebar-tag-input-suggest-empty').textContent).toBe('无匹配标签')
    expect(screen.queryByTestId('sidebar-tag-input-suggest-create')).toBeNull()
  })
})
