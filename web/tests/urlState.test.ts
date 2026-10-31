/**
 * URL 单一真源单测（URL-STATE-1 / 卡 task）：序列化往返 / 默认值回退 / 稳定 key 顺序 /
 * 深链构造 / 偏好清除语义。纯函数层（不渲染 React，路由钩子的 URL 优先逻辑由 e2e 走查覆盖）。
 *   pnpm -C web test
 */
import { describe, expect, it } from 'vitest'

import {
  activeKeys,
  applyRecord,
  decodeValue,
  encodeValue,
  isDefaultRecord,
  orderParams,
  queryOf,
  readRecord,
  withParams,
} from '../src/lib/urlState'

describe('URL 值编解码', () => {
  it('标量往返：string / number / boolean', () => {
    expect(encodeValue('雪山')).toBe('雪山')
    expect(encodeValue(24)).toBe('24')
    expect(encodeValue(true)).toBe('1')
    expect(encodeValue(false)).toBe('0')
    expect(decodeValue('雪山', '')).toBe('雪山')
    expect(decodeValue('24', 0)).toBe(24)
    expect(decodeValue('1', false)).toBe(true)
    expect(decodeValue('0', true)).toBe(false)
  })

  it('空值语义：空串 / null / undefined / 空数组 = 删除该键', () => {
    expect(encodeValue('')).toBeNull()
    expect(encodeValue(null)).toBeNull()
    expect(encodeValue(undefined)).toBeNull()
    expect(encodeValue([])).toBeNull()
    expect(encodeValue(Number.NaN)).toBeNull()
  })

  it('选择集：number[] ↔ 逗号分隔，非法项被丢弃', () => {
    expect(encodeValue([12, 34, 56])).toBe('12,34,56')
    expect(decodeValue('12,34,56', [] as number[])).toEqual([12, 34, 56])
    expect(decodeValue('12,abc,-3,0,56', [] as number[])).toEqual([12, 56])
    expect(decodeValue('', [1, 2])).toEqual([1, 2]) // 空串 → 默认值
  })

  it('脏链接不炸：类型不符/JSON 坏掉一律回退默认值', () => {
    expect(decodeValue('abc', 3)).toBe(3)
    expect(decodeValue('NaN', 3)).toBe(3)
    expect(decodeValue('{oops', { a: 1 })).toEqual({ a: 1 })
  })
})

describe('记录读写', () => {
  const DEFAULTS = {
    q: '',
    tab: 'all',
    path: '',
    tags: '',
    rating: 0,
    untagged: false,
    sort: '',
    dir: '',
    ids: [] as number[],
  }

  it('缺键 = 默认值（URL 优先，不污染其它键）', () => {
    const params = new URLSearchParams('?q=雪山&path=/photos/2024&rating=4&untagged=1')
    expect(readRecord(params, DEFAULTS)).toEqual({
      ...DEFAULTS,
      q: '雪山',
      path: '/photos/2024',
      rating: 4,
      untagged: true,
    })
  })

  it('写回只保留非默认键，并保留本记录之外的参数', () => {
    const params = new URLSearchParams('?asset=42&q=old&path=/a')
    const next = applyRecord(params, { ...DEFAULTS, q: '雪山', tab: 'recent' }, DEFAULTS)
    expect(next.get('q')).toBe('雪山')
    expect(next.get('tab')).toBe('recent')
    expect(next.has('path')).toBe(false) // 回到默认 → 键被删掉
    expect(next.get('asset')).toBe('42') // 别人的参数不动
    expect(next.toString()).toBe('q=%E9%9B%AA%E5%B1%B1&asset=42&tab=recent')
  })

  it('稳定 key 顺序：同一状态不同插入顺序 → 同一串', () => {
    const a = new URLSearchParams('?ids=2,1&q=x&open=export')
    const b = new URLSearchParams('?open=export&q=x&ids=2,1')
    expect(queryOf(a)).toBe(queryOf(b))
    expect(queryOf(a)).toBe('?q=x&open=export&ids=2%2C1')
  })

  it('未知参数按字母序排到尾部（不丢）', () => {
    const params = new URLSearchParams('?zzz=1&aaa=2&q=x')
    expect(queryOf(params)).toBe('?q=x&aaa=2&zzz=1')
  })

  it('activeKeys / isDefaultRecord：判断「有没有筛选」', () => {
    expect(activeKeys(DEFAULTS, DEFAULTS)).toEqual([])
    expect(isDefaultRecord(DEFAULTS, DEFAULTS)).toBe(true)
    const withFilter = { ...DEFAULTS, tags: '海边', rating: 3 }
    expect(activeKeys(withFilter, DEFAULTS)).toEqual(['tags', 'rating'])
    expect(isDefaultRecord(withFilter, DEFAULTS)).toBe(false)
  })
})

describe('深链构造（跨区跳转）', () => {
  it('打补丁 + 稳定顺序；null/空串 = 删键', () => {
    expect(withParams('/', { q: '雪山', sort: 'name', dir: 'asc' })).toBe(
      '/?q=%E9%9B%AA%E5%B1%B1&sort=name&dir=asc',
    )
    expect(withParams('/search?q=x&mode=text', { mode: 'image', q: '' })).toBe(
      '/search?mode=image',
    )
    expect(withParams('/?ids=1,2', { ids: [3] })).toBe('/?ids=3')
  })

  it('深链往返：图库筛选 + 浮层 + 选择集', () => {
    const url = withParams('/', {
      q: '猫',
      tab: 'all',
      tags: '宠物',
      rating: 4,
      open: 'export',
      ids: [101, 102],
    })
    const params = new URLSearchParams(url.split('?')[1])
    const got = readRecord(params, {
      q: '',
      tab: 'all',
      tags: '',
      rating: 0,
      open: '',
      ids: [] as number[],
    })
    expect(got).toEqual({ q: '猫', tab: 'all', tags: '宠物', rating: 4, open: 'export', ids: [101, 102] })
    // withParams 是 schema 无关的打补丁（不知道默认值）→ 显式默认值也会写进去；
    // 「默认值不进 URL」由 applyRecord 保证（见上一组用例）
    expect(params.get('tab')).toBe('all')
  })
})

describe('偏好补齐语义（localStorage 只当「裸链接」的默认值）', () => {
  // ids 也在记录里：图库页 ?ids= 的深链同样算「显式链接」，不该被个人偏好改味
  const DEFAULTS = { q: '', path: '', rating: 0, ids: [] as number[] }

  /** 模拟钩子首挂载时的合并规则：URL 有任一键 = 显式链接 → 偏好整体不参与 */
  const seed = (url: string, pref: Record<string, unknown> | null) => {
    const params = new URLSearchParams(url)
    const out: Record<string, unknown> = { ...readRecord(params, DEFAULTS) }
    const applied: string[] = []
    const explicit = Object.keys(DEFAULTS).some((k) => params.has(k))
    if (!explicit && pref) {
      for (const k of Object.keys(DEFAULTS)) {
        const v = pref[k]
        if (v === undefined || encodeValue(v) === null) continue
        if (encodeValue(v) === encodeValue(DEFAULTS[k as keyof typeof DEFAULTS])) continue
        out[k] = v
        applied.push(k)
      }
    }
    return { value: out, applied }
  }

  it('裸链接 → 用偏好补，并标记来源（补完写回 URL）', () => {
    const s = seed('', { q: '雪山', rating: 5, path: '' })
    expect(s.value).toEqual({ q: '雪山', path: '', rating: 5, ids: [] })
    expect(s.applied).toEqual(['q', 'rating'])
    const back = applyRecord(new URLSearchParams(''), s.value as never, DEFAULTS)
    expect(back.toString()).toBe('q=%E9%9B%AA%E5%B1%B1&rating=5')
  })

  it('显式链接（URL 里有任一键）→ 偏好整体不参与，不显示来源提示', () => {
    const s = seed('?q=%E7%8C%AB&rating=0', { q: '雪山', rating: 5 })
    expect(s.value).toEqual({ q: '猫', path: '', rating: 0, ids: [] })
    expect(s.applied).toEqual([])
    // 只带浮层/选择集的深链也不被个人偏好改味
    const ov = seed('?ids=1,2', { q: '雪山', rating: 5 })
    expect(ov.applied).toEqual([])
    expect(ov.value).toEqual({ q: '', path: '', rating: 0, ids: [1, 2] })
  })

  it('偏好的值与默认值相同（view:"all"/rating:0）不算生效，不弹横幅', () => {
    expect(seed('', { q: '', rating: 0 }).applied).toEqual([])
  })

  it('清除偏好 = 键回默认、URL 参数消失', () => {
    const cleared = applyRecord(new URLSearchParams('?q=%E9%9B%AA%E5%B1%B1&rating=5'), DEFAULTS, DEFAULTS)
    expect(cleared.toString()).toBe('')
    expect(isDefaultRecord(readRecord(cleared, DEFAULTS), DEFAULTS)).toBe(true)
  })
})
