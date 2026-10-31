import { beforeEach, describe, expect, it } from 'vitest'
import { hitOrder, hitToAsset, orderByHits } from '../src/lib/searchResults'
import * as S from '../src/lib/selection'
import { clearPendingSet, peekPendingSet, stashPendingSet } from '../src/lib/tempSet'
import type { AssetView, SearchHit } from '../src/lib/api'

/**
 * 卡 task 的纯逻辑证据（node 环境，无 DOM）：
 * - 命中顺序 → 视图顺序（相似度降序不能被后端 importedAt 顺序打乱）；
 * - 回读失败/缺失时用占位视图补齐，不静默丢结果；
 * - 选择模型（lib/selection）在「搜索结果」这个视图上语义与图库一致；
 * - 搜索结果 → 图库 的临时集合交接（localStorage 读写即清）。
 */

const HITS: SearchHit[] = [
  { assetId: 7, resourceId: 1, score: 0.99, fileName: 'a.jpg', titleCh: null, thumbPath: null, width: 100, height: 100 },
  { assetId: 3, resourceId: 2, score: 0.95, fileName: 'b.jpg', titleCh: null, thumbPath: null, width: 100, height: 100 },
  { assetId: 9, resourceId: 3, score: 0.92, fileName: 'c.jpg', titleCh: null, thumbPath: null, width: 100, height: 100 },
]

function full(id: number): AssetView {
  return { ...hitToAsset(HITS.find((h) => h.assetId === id)!), titleCh: `真实标题 ${id}`, starRating: 3 }
}

describe('searchResults：命中 → 视图', () => {
  it('hitOrder 保留命中（相似度）顺序', () => {
    expect(hitOrder(HITS)).toEqual([7, 3, 9])
  })

  it('orderByHits 按命中顺序重排（后端返回的顺序不参与）', () => {
    const out = orderByHits(HITS, [full(9), full(7), full(3)])
    expect(out.map((a) => a.assetId)).toEqual([7, 3, 9])
    expect(out[0].titleCh).toBe('真实标题 7')
  })

  it('回读缺失的命中用占位补齐（不丢结果、字段来自命中本身）', () => {
    const out = orderByHits(HITS, [full(7)])
    expect(out).toHaveLength(3)
    expect(out[1].assetId).toBe(3)
    expect(out[1].fileName).toBe('b.jpg')
    expect(out[1].approvalStatus).toBe('pending')
    expect(out[2].starRating).toBe(0)
  })
})

describe('搜索结果上的选择模型（复用 lib/selection）', () => {
  const order = hitOrder(HITS)

  it('全选 → 选择集长度 = 结果数，顺序 = 视图顺序', () => {
    const sel = S.all(order)
    expect(sel.ids).toEqual([7, 3, 9])
    expect(sel.ids).toHaveLength(HITS.length)
    expect(S.targetIds(sel)).toEqual([7, 3, 9])
  })

  it('单击单选 / cmd 加选 / shift 范围选（范围选按视图序而非点击顺序）', () => {
    let sel = S.single(9)
    expect(sel.ids).toEqual([9])
    sel = S.toggle(sel, order, 7)
    expect(sel.ids).toEqual([7, 9]) // 视图顺序（7 在 9 之前）
    sel = S.range(order, { ...sel, anchor: 7 }, 3)
    expect(sel.ids).toEqual([7, 3])
    expect(sel.active).toBe(3)
  })

  it('Esc 清空；无选择时批量动作落到游标（与图库一致）', () => {
    expect(S.clear().ids).toEqual([])
    expect(S.targetIds({ ids: [], anchor: 3, active: 3 })).toEqual([3])
    expect(S.targetIds(S.EMPTY)).toEqual([])
  })
})

describe('tempSet：搜索结果 → 图库 的临时集合交接', () => {
  beforeEach(() => {
    const store = new Map<string, string>()
    // node 环境没有 localStorage：给最小实现（只测键值读写语义）
    ;(globalThis as unknown as { localStorage: Storage }).localStorage = {
      getItem: (k: string) => store.get(k) ?? null,
      setItem: (k: string, v: string) => void store.set(k, v),
      removeItem: (k: string) => void store.delete(k),
      clear: () => store.clear(),
      key: (i: number) => [...store.keys()][i] ?? null,
      get length() {
        return store.size
      },
    } as Storage
  })

  it('stash → peek 读到同一集合；clear 后为空', () => {
    stashPendingSet({ label: '搜索「雪山」的 3 张结果', ids: [7, 3, 9] })
    expect(peekPendingSet()).toEqual({ label: '搜索「雪山」的 3 张结果', ids: [7, 3, 9] })
    clearPendingSet()
    expect(peekPendingSet()).toBeNull()
  })

  it('非法/空集合不被当成有效交接（不打开空临时集合）', () => {
    localStorage.setItem('dam.pendingSet', JSON.stringify({ label: 'x', ids: [] }))
    expect(peekPendingSet()).toBeNull()
    localStorage.setItem('dam.pendingSet', 'not-json')
    expect(peekPendingSet()).toBeNull()
  })
})
