import { ocrBadge, ocrHint, type OcrHint } from '../lib/ocrChip'
import { useI18n } from '../i18n'

/**
 * 「文字命中」角标（OCR-MATCH，卡 t_df883cdd）：搜索命中项渲染 OCR 字面证据。
 *
 * 单独成组件的理由：判定口径（`ocrChip.ts`）是纯函数可单测，但**渲染出的 DOM 文本**
 * 也要能程序化取到（卡面要求「语义召回 vs 文字命中」的来源区分证据 = DOM 文本而非口头），
 * 所以这里是个无 hooks 的纯展示组件 —— vitest 里 `renderToStaticMarkup` 就能出 HTML 存证，
 * 不需要 jsdom。
 */
export function OcrChip({ hit }: { hit: OcrHint }) {
  const { t } = useI18n()
  const label = ocrBadge(hit)
  if (!label) return null
  return (
    <span
      data-testid="ocr-chip"
      className="absolute top-1 right-1 rounded bg-sky-600/90 px-1.5 py-0.5 text-[10px] font-semibold text-white"
      title={t('OCR 文字命中：图上识别文本里出现查询词（不是语义相似）')}
    >
      {label}
    </span>
  )
}

/** 命中片段（角标下方可选用；空则不渲染）。 */
export function OcrSnippetLine({ hit }: { hit: OcrHint }) {
  const hint = ocrHint(hit)
  if (!hint) return null
  return (
    <span data-testid="ocr-snippet" className="truncate text-[10px] text-sky-300">
      {hint}
    </span>
  )
}
