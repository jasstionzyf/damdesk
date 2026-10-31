/**
 * E2 选择模型单测（vitest；W7 起统一测试栈——旧 node --test 版随桌面 invoke 前端退役）。
 * 覆盖：单选/加选/范围选/全选/移动/边界/筛选/打分目标。
 *   pnpm -C web test
 */
import { describe, expect, it } from 'vitest'

import * as S from '../src/lib/selection'

const ORDER = [10, 20, 30, 40, 50]

describe('E2 选择模型', () => {
  it('单选：清掉其它选择，anchor/active 同步', () => {
    const st = S.single(30)
    expect(st).toEqual({ ids: [30], anchor: 30, active: 30 })
  })

  it('加选：toggle 增删且保持视图顺序', () => {
    let st = S.single(20)
    st = S.toggle(st, ORDER, 50)
    expect(st.ids).toEqual([20, 50])
    st = S.toggle(st, ORDER, 10)
    expect(st.ids).toEqual([10, 20, 50])
    st = S.toggle(st, ORDER, 20)
    expect(st.ids).toEqual([10, 50])
    expect(st.active).toBe(20)
  })

  it('范围选：anchor→目标，两个方向都成立', () => {
    let st = S.single(20)
    st = S.range(ORDER, st, 50)
    expect(st.ids).toEqual([20, 30, 40, 50])
    expect(st.anchor).toBe(20)
    expect(st.active).toBe(50)
    const back = S.range(ORDER, { ...st, anchor: 40 }, 10)
    expect(back.ids).toEqual([10, 20, 30, 40])
  })

  it('范围选：anchor 不在视图里时退化为单选', () => {
    const st = S.range(ORDER, S.single(999), 30)
    expect(st.ids).toEqual([30])
  })

  it('全选 / 清空', () => {
    expect(S.all(ORDER).ids).toEqual(ORDER)
    expect(S.all([])).toEqual(S.EMPTY)
    expect(S.clear()).toEqual(S.EMPTY)
    expect(S.isOn(S.single(10), 10)).toBe(true)
    expect(S.isOn(S.single(10), 20)).toBe(false)
  })

  it('游标移动：夹取边界不循环，单选', () => {
    let st = S.move(S.single(10), ORDER, 1)
    expect(st.ids).toEqual([20])
    st = S.move(st, ORDER, 1)
    expect(st.ids).toEqual([30])
    st = S.move(st, ORDER, -1)
    expect(st.ids).toEqual([20])
    const atStart = S.single(10)
    expect(S.move(atStart, ORDER, -1)).toBe(atStart)
  })

  it('shift+方向键：范围扩展（anchor 不动）', () => {
    let st = S.single(20)
    st = S.move(st, ORDER, 1, true)
    expect(st.ids).toEqual([20, 30])
    st = S.move(st, ORDER, 1, true)
    expect(st.ids).toEqual([20, 30, 40])
    expect(st.anchor).toBe(20)
    st = S.move(st, ORDER, -1, true)
    expect(st.ids).toEqual([20, 30])
  })

  it('视图为空时的退化行为', () => {
    expect(S.move(S.EMPTY, [], 1)).toEqual(S.EMPTY)
    expect(S.navIndex(0, 0, 1)).toBe(0)
  })

  it('Lightbox 翻页下标夹取', () => {
    expect(S.navIndex(5, 0, -1)).toBe(0)
    expect(S.navIndex(5, 4, 1)).toBe(4)
    expect(S.navIndex(5, 2, 1)).toBe(3)
  })

  it('快速筛选：文件名/标题大小写不敏感，空串返回全量', () => {
    const items = [
      { assetId: 1, fileName: 'Sunset.JPG', titleCh: '日落' },
      { assetId: 2, fileName: 'street.png', titleEn: 'street market' },
      { assetId: 3, fileName: 'x.jpg', description: 'Dusk over sea' },
    ]
    expect(S.filterByQuery(items, '').length).toBe(3)
    expect(S.filterByQuery(items, 'sunset').map((i) => i.assetId)).toEqual([1])
    expect(S.filterByQuery(items, ' dusk ').map((i) => i.assetId)).toEqual([3])
    expect(S.filterByQuery(items, 'MARKET').map((i) => i.assetId)).toEqual([2])
    expect(S.filterByQuery(items, 'nothing')).toEqual([])
  })

  it('打分目标：选择集优先，缺省落到游标', () => {
    expect(S.targetIds(S.single(7))).toEqual([7])
    expect(S.targetIds({ ids: [1, 2], anchor: 1, active: 2 })).toEqual([1, 2])
    expect(S.targetIds({ ids: [], anchor: null, active: 9 })).toEqual([9])
    expect(S.targetIds(S.EMPTY)).toEqual([])
  })
})
