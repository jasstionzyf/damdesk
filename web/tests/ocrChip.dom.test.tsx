import { describe, expect, it } from 'vitest'
import { renderToStaticMarkup } from 'react-dom/server'
import { OcrChip, OcrSnippetLine } from '../src/components/OcrChip'

/**
 * 「语义召回 vs 文字命中」的来源区分证据（DOM 文本，卡 t_df883cdd 卡面要求）：
 * 同一批搜索命中里，纯语义命中不渲染任何字面标记，字面命中渲染「文字命中」+ 片段 ——
 * 这里用 react-dom/server 把组件渲染成 HTML，断言里带的就是可核对 DOM 文本。
 */
describe('OcrChip / OcrSnippetLine 渲染（DOM 文本存证）', () => {
  it('文字命中 → 渲染「文字命中」角标 + 片段行', () => {
    const hit = { ocrMatched: true, ocrSnippet: '发票 报销单 模板' }
    const chip = renderToStaticMarkup(<OcrChip hit={hit} />)
    const line = renderToStaticMarkup(<OcrSnippetLine hit={hit} />)
    console.log('[DOM] chip =', chip)
    console.log('[DOM] snippet =', line)
    expect(chip).toContain('data-testid="ocr-chip"')
    expect(chip).toContain('>文字命中</span>')
    expect(line).toContain('data-testid="ocr-snippet"')
    expect(line).toContain('文字命中：发票 报销单 模板')
  })

  it('纯语义命中（ocrMatched=false）→ 两个组件都渲染空（来源可区分）', () => {
    const hit = { ocrMatched: false, ocrSnippet: null }
    const chip = renderToStaticMarkup(<OcrChip hit={hit} />)
    const line = renderToStaticMarkup(<OcrSnippetLine hit={hit} />)
    console.log('[DOM] 纯语义命中 chip =', JSON.stringify(chip), 'snippet =', JSON.stringify(line))
    expect(chip).toBe('')
    expect(line).toBe('')
  })

  it('老后端（无 ocrMatched 字段）→ 不渲染（向后兼容不误标）', () => {
    expect(renderToStaticMarkup(<OcrChip hit={{}} />)).toBe('')
    expect(renderToStaticMarkup(<OcrSnippetLine hit={{}} />)).toBe('')
  })

  it('长片段在 DOM 里按字符截断（不切断汉字/代理对）', () => {
    const hit = { ocrMatched: true, ocrSnippet: `${'前'.repeat(100)}发票` }
    const html = renderToStaticMarkup(<OcrSnippetLine hit={hit} />)
    console.log('[DOM] long snippet =', html.slice(0, 120), '…')
    expect(html).toContain('…')
    expect(html).not.toContain('前'.repeat(61))
  })
})
