import { describe, expect, it } from 'vitest'
import { ocrBadge, ocrHint, ocrSnippetOf } from '../src/lib/ocrChip'

describe('ocrBadge / ocrHint / ocrSnippetOf（OCR-MATCH 前端展示口径）', () => {
  it('字面命中 → 角标「文字命中」+ 提示带片段', () => {
    const hit = { ocrMatched: true, ocrSnippet: '发票号 A-001 合计 1200 元' }
    expect(ocrBadge(hit)).toBe('文字命中')
    expect(ocrHint(hit)).toBe('文字命中：发票号 A-001 合计 1200 元')
    expect(ocrSnippetOf(hit)).toBe('发票号 A-001 合计 1200 元')
  })

  it('纯语义命中（ocrMatched=false 或字段缺省）→ 一律不标', () => {
    expect(ocrBadge({ ocrMatched: false, ocrSnippet: null })).toBeNull()
    expect(ocrHint({ ocrMatched: false, ocrSnippet: null })).toBeNull()
    // 老后端没有这两个字段（向后兼容）：不得误标
    expect(ocrBadge({})).toBeNull()
    expect(ocrHint({})).toBeNull()
    expect(ocrSnippetOf({})).toBeNull()
  })

  it('命中但片段为空 → 角标仍在，提示退化为「文字命中」', () => {
    expect(ocrBadge({ ocrMatched: true, ocrSnippet: '' })).toBe('文字命中')
    expect(ocrHint({ ocrMatched: true, ocrSnippet: '   ' })).toBe('文字命中')
    expect(ocrSnippetOf({ ocrMatched: true, ocrSnippet: null })).toBeNull()
  })

  it('长片段按字符截断并加省略号（不切断代理对/汉字）', () => {
    const long = '前'.repeat(60) + '发票'
    const s = ocrSnippetOf({ ocrMatched: true, ocrSnippet: long }, 10)
    expect([...s!].length).toBe(11) // 10 字符 + 省略号
    expect(s!.endsWith('…')).toBe(true)
    const emoji = ocrSnippetOf({ ocrMatched: true, ocrSnippet: '👍'.repeat(20) }, 3)
    expect([...emoji!]).toEqual(['👍', '👍', '👍', '…'])
  })

  it('非字面命中即使带了片段也不展示（以后端标记为准）', () => {
    expect(ocrHint({ ocrMatched: false, ocrSnippet: '不该出现' })).toBeNull()
    expect(ocrSnippetOf({ ocrMatched: false, ocrSnippet: '不该出现' })).toBeNull()
  })
})
