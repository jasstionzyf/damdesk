import { describe, expect, it } from 'vitest'
import {
  describeBrowseFilters,
  filterKey,
  normalizeTagQuery,
  searchImageQuery,
  TEXT_MIN_SCORE,
} from '../src/lib/searchQuery'

/**
 * AUDIT-B2 §1 修复的单元存证：上传图搜索的 k / 过滤 / minScore 只能走 query，
 * 修好前后端 by_image 只接 minScore（选条数、填扩展名都不生效）。
 */
describe('searchImageQuery（上传图搜索的 query 串）', () => {
  it('无参数 → 空串（不拼一个孤零零的 ?）', () => {
    expect(searchImageQuery()).toBe('')
    expect(searchImageQuery({})).toBe('')
  })

  it('k 与 minScore 写进 query', () => {
    expect(searchImageQuery({ k: 12 })).toBe('?k=12')
    expect(searchImageQuery({ k: 48, minScore: 0.5 })).toBe('?k=48&minScore=0.5')
    // minScore=0 是「关闭过滤」，不能被当成 falsy 丢掉
    expect(searchImageQuery({ minScore: 0 })).toBe('?minScore=0')
  })

  it('标量过滤逐项写入，空串/undefined 不写（后端会把空串当「只要空名」）', () => {
    const qs = searchImageQuery({
      k: 24,
      filter: { tag: '猫', dirPrefix: '/photos', ext: 'png', minWidth: 800 },
    })
    const q = new URLSearchParams(qs)
    expect(q.get('k')).toBe('24')
    expect(q.get('tag')).toBe('猫')
    expect(q.get('dirPrefix')).toBe('/photos')
    expect(q.get('ext')).toBe('png')
    expect(q.get('minWidth')).toBe('800')
    expect(qs).not.toContain('minHeight')

    const blank = new URLSearchParams(searchImageQuery({ filter: { tag: '  ', ext: '' } }))
    expect(blank.toString()).toBe('')
  })

  it('中文/斜杠等字符做 URL 编码（query 可被后端原样解出）', () => {
    const qs = searchImageQuery({ filter: { tag: '婚礼 & 对戒', dirPrefix: '/a b/c' } })
    const q = new URLSearchParams(qs)
    expect(q.get('tag')).toBe('婚礼 & 对戒')
    expect(q.get('dirPrefix')).toBe('/a b/c')
    expect(qs).not.toContain(' ')
  })
})

describe('filterKey（条件指纹，搜索页用它决定要不要自动重搜）', () => {
  it('同一份条件同 key；任一项变化 → key 变化', () => {
    const base = filterKey(24, {})
    expect(filterKey(24, {})).toBe(base)
    expect(filterKey(48, {})).not.toBe(base)
    expect(filterKey(24, { ext: 'png' })).not.toBe(base)
    expect(filterKey(24, { tag: '猫' })).not.toBe(base)
    expect(filterKey(24, { dirPrefix: '/x' })).not.toBe(base)
    expect(filterKey(48, { ext: 'png' })).not.toBe(filterKey(24, { ext: 'png' }))
  })
})

describe('TEXT_MIN_SCORE', () => {
  it('与后端 svc-library::DEFAULT_TEXT_MIN_SCORE 对齐（0.085）', () => {
    expect(TEXT_MIN_SCORE).toBeCloseTo(0.085, 6)
  })
})

describe('normalizeTagQuery（task，与 core_types::normalize_tag_query 同口径）', () => {
  it('trim 首尾空白 + 内部空白串折叠成 _', () => {
    expect(normalizeTagQuery('  尺寸-滚筒_10KG  ')).toBe('尺寸-滚筒_10KG')
    expect(normalizeTagQuery('滚筒 10KG')).toBe('滚筒_10KG')
    expect(normalizeTagQuery('a \t b   c')).toBe('a_b_c')
    expect(normalizeTagQuery('   ')).toBe('')
  })

  it('大小写不动（后端 SQL COLLATE NOCASE 处理，走 nocase 索引）', () => {
    expect(normalizeTagQuery('Sunset')).toBe('Sunset')
  })
})

describe('describeBrowseFilters（纯过滤浏览的条件描述）', () => {
  it('只列非空条件；标签归一化、扩展名小写', () => {
    expect(describeBrowseFilters('', '', '')).toBe('')
    expect(describeBrowseFilters(' 尺寸-滚筒 10KG ', '', '')).toBe('标签「尺寸-滚筒_10KG」')
    expect(describeBrowseFilters('', '/lib/aa', ' JPG ')).toBe('目录 /lib/aa · 扩展名 jpg')
  })
})
