/**
 * 网格密度单测（UI-LAYOUT / 卡 t_11662290）
 * 覆盖 soujpg-ui-patterns §9.6 的**数字判据**：列数随容器宽度算、≥1280/≥4 列且列宽 ≥170px、
 * 1000-1279 ≥3 列、永不溢出（列数×列宽+间距 ≤ 容器宽）。纯函数层，不渲染 React
 * （URL 真源 / 偏好兜底由 CDP e2e 走查覆盖）。
 *   pnpm -C web test
 */
import { describe, expect, it } from 'vitest'

import {
  DENSITY_PRESETS,
  DEFAULT_THUMB,
  GRID_GAP,
  THUMB_MAX,
  THUMB_MIN,
  clampThumb,
  columnsFor,
  presetOf,
} from '../src/components/DensityControl'

/** 按 CSS auto-fill 的语义反算「实际列宽」（1fr 均分剩余空间） */
function colWidth(width: number, thumb: number, gap: number = GRID_GAP): number {
  const n = columnsFor(width, thumb, gap)
  return (width - (n - 1) * gap) / n
}

describe('密度值域', () => {
  it('三档标称值单调递增，且都在合法区间内', () => {
    const thumbs = DENSITY_PRESETS.map((p) => p.thumb)
    expect(thumbs).toEqual([...thumbs].sort((a, b) => a - b))
    for (const t of thumbs) expect(t).toBeGreaterThanOrEqual(THUMB_MIN)
    for (const t of thumbs) expect(t).toBeLessThanOrEqual(THUMB_MAX)
  })

  it('默认档 = 170px（§9.6 的列宽下限）', () => {
    expect(DEFAULT_THUMB).toBe(170)
    expect(DENSITY_PRESETS.find((p) => p.key === 'M')?.thumb).toBe(DEFAULT_THUMB)
  })

  it('clampThumb：越界夹紧、非法回默认、小数取整', () => {
    expect(clampThumb(50)).toBe(THUMB_MIN)
    expect(clampThumb(9999)).toBe(THUMB_MAX)
    expect(clampThumb('180')).toBe(180)
    expect(clampThumb('abc')).toBe(DEFAULT_THUMB)
    expect(clampThumb(Number.NaN)).toBe(DEFAULT_THUMB)
    expect(clampThumb(null)).toBe(DEFAULT_THUMB)
    expect(clampThumb(170.6)).toBe(171)
  })

  it('presetOf：命中标称档返回档位，滑块任意值不假装命中', () => {
    expect(presetOf(130)).toBe('S')
    expect(presetOf(170)).toBe('M')
    expect(presetOf(220)).toBe('L')
    expect(presetOf(181)).toBe('')
  })
})

describe('columnsFor：与 CSS auto-fill 语义逐字对齐', () => {
  it('无效宽度返回 0 列（还没布局，不谎报 1 列）', () => {
    expect(columnsFor(0, 170)).toBe(0)
    expect(columnsFor(Number.NaN, 170)).toBe(0)
    expect(columnsFor(-100, 170)).toBe(0)
  })

  it('列数 × 列宽 + 间距 永远不超过容器宽（不溢出）', () => {
    for (const width of [300, 480, 604, 784, 860, 1024, 1280, 1600]) {
      for (const p of DENSITY_PRESETS) {
        const n = columnsFor(width, p.thumb)
        expect(n).toBeGreaterThanOrEqual(1)
        const need = n * p.thumb + (n - 1) * GRID_GAP
        expect(need).toBeLessThanOrEqual(width)
        // 再加一列就放不下（否则就不是"尽可能多列"）
        const extra = (n + 1) * p.thumb + n * GRID_GAP
        expect(extra).toBeGreaterThan(width)
      }
    }
  })

  it('§9.6 ≥1280 判据：主内容 784px 时默认档 4 列且列宽 ≥170px', () => {
    const width = 784 // 1280 - 导航 208 - 库面板 240 - 检查器竖条 48
    const n = columnsFor(width, DEFAULT_THUMB)
    expect(n).toBeGreaterThanOrEqual(4)
    expect(colWidth(width, DEFAULT_THUMB)).toBeGreaterThanOrEqual(170)
  })

  it('§9.6 1000-1279 判据：主内容 604px 时 ≥3 列（默认档）', () => {
    const width = 604 // 1100 - 导航 208 - 检查器 288（库面板已转抽屉）
    expect(columnsFor(width, DEFAULT_THUMB)).toBeGreaterThanOrEqual(3)
  })

  it('密度直接影响列数：同宽下 S 比 M 多列、L 比 M 少列', () => {
    const width = 784
    const s = columnsFor(width, 130)
    const m = columnsFor(width, DEFAULT_THUMB)
    const l = columnsFor(width, 220)
    expect(s).toBeGreaterThanOrEqual(m)
    expect(m).toBeGreaterThan(l)
    expect(s).toBe(Math.floor((width + GRID_GAP) / (130 + GRID_GAP)))
  })
})
