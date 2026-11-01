import { tGlobal } from '../i18n'
import type { SearchHit } from './api'

/**
 * 搜索命中的「OCR 字面命中」展示口径（OCR-MATCH，卡 t_df883cdd）。
 *
 * 为什么单独抽成纯函数：命中卡片是 DOM 组件（vitest 这里跑 node 环境，不装 jsdom），
 * 把「标不标、标什么、片段怎么截」的判定抽出来才可被单测覆盖 —— 也顺便保证
 * ocrMatched/ocrSnippet 这两个后端新字段在前端不是「接了不用」的死字段。
 */

/** 命中里与字面证据有关的字段（后端 SearchHit 的子集）。 */
export type OcrHint = Pick<SearchHit, 'ocrMatched' | 'ocrSnippet'>

/** 缩略图角标：字面命中 → '文字命中'；纯语义命中 → null（不标，不猜）。 */
export function ocrBadge(hit: OcrHint): string | null {
  return hit.ocrMatched ? tGlobal('文字命中') : null
}

/** 命中片段（按字符截断，超出加省略号；空串当无）。 */
export function ocrSnippetOf(hit: OcrHint, max = 48): string | null {
  const s = (hit.ocrSnippet ?? '').trim()
  if (!hit.ocrMatched || s === '') return null
  const chars = [...s]
  return chars.length <= max ? s : `${chars.slice(0, max).join('')}…`
}

/** 悬停提示行：'文字命中：<片段>'；命中但无片段 → '文字命中'；非字面命中 → null。 */
export function ocrHint(hit: OcrHint): string | null {
  if (!hit.ocrMatched) return null
  const s = ocrSnippetOf(hit, 60)
  return s ? tGlobal('文字命中：{s}', { s }) : tGlobal('文字命中')
}
