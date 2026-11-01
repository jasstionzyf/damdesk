import { describe, expect, it } from 'vitest'
import { renderToStaticMarkup } from 'react-dom/server'
import { SimilarScoreLine } from '../src/components/Inspector'

/**
 * W3 以图搜图相关性过滤的界面存证（卡 t_d00827d5）：
 * 命中行显示「相似度 x%」，并标明本次生效下限；未标定模型（后端不过滤）不显示分母，
 * 避免给出一个并不存在的阈值承诺。
 */
describe('SimilarScoreLine（找相似命中行的相似度）', () => {
  it('已标定模型 → 相似度 + 本次下限都渲染', () => {
    const html = renderToStaticMarkup(<SimilarScoreLine score={0.8451} min={0.72} />)
    console.log('[DOM]', html)
    expect(html).toContain('data-testid="insp-similar-score"')
    expect(html).toContain('相似度 85%')
    expect(html).toContain('本次下限 72%')
  })

  it('未标定模型（min=null/0）→ 只显示相似度，不显示下限', () => {
    for (const min of [null, 0, undefined]) {
      const html = renderToStaticMarkup(<SimilarScoreLine score={0.5} min={min} />)
      expect(html).toContain('相似度 50%')
      expect(html).not.toContain('本次下限')
    }
  })
})
