import type { SearchFilterReq } from './api'
import { tGlobal } from '../i18n'

/**
 * `/api/search/image` 的 query 串构造（AUDIT-B2 §1 修复）：
 * 上传路径的 body 是图片字节流，所以 k / 过滤 / minScore 只能走 query。
 * 修好之前后端 `by_image` 只接 minScore —— 填 ext=png 仍返回 24 条（含 jpg）、条数选择无效。
 *
 * 抽成纯函数是为了能直接单测（不起 DOM）：空串/undefined 的过滤项一律不写，
 * 免得后端把空串当成"只要空名"从而搜不到东西。
 */
export function searchImageQuery(opts?: {
  k?: number
  filter?: SearchFilterReq
  minScore?: number
}): string {
  const q = new URLSearchParams()
  if (opts?.k !== undefined) q.set('k', String(opts.k))
  if (opts?.minScore !== undefined) q.set('minScore', String(opts.minScore))
  const f = opts?.filter
  if (f) {
    for (const key of [
      'tag',
      'dirPrefix',
      'ext',
      'minWidth',
      'minHeight',
      'dateFrom',
      'dateTo',
    ] as const) {
      const v = f[key]
      if (v === undefined || v === null) continue
      if (typeof v === 'string' && v.trim() === '') continue
      q.set(key, String(v))
    }
  }
  const s = q.toString()
  return s ? `?${s}` : ''
}

/** 过滤条件指纹：搜索页用它判断「条件变了要不要重搜」 */
export function filterKey(k: number, f: SearchFilterReq): string {
  return [k, f.dirPrefix ?? '', f.ext ?? '', f.tag ?? '', f.minWidth ?? '', f.minHeight ?? ''].join('|')
}

/** 文搜图默认相关性下限（后端 svc-library::DEFAULT_TEXT_MIN_SCORE）；低于它的命中被隐藏 */
export const TEXT_MIN_SCORE = 0.085

/**
 * 严格档 = 默认 × 1.5（KI-16 #4 三档映射；四舍五入到 4 位小数保证 URL 序列化往返稳定）。
 * 宽松档 = 0（显式传 0 = 后端不过滤）；默认档 = 不传（后端按模型标定兜底）。
 */
export const STRICT_TEXT_MIN_SCORE = Math.round(TEXT_MIN_SCORE * 1.5 * 10000) / 10000

export type TextMinLevel = 'strict' | 'default' | 'loose' | 'custom'

/** URL `?simMin=` 数值 → 档位（缺省/非法/负数哨兵 → default） */
export function textMinLevel(v: number | undefined | null): TextMinLevel {
  if (v === undefined || v === null || Number.isNaN(v) || v < 0) return 'default'
  if (v === 0) return 'loose'
  if (Math.abs(v - STRICT_TEXT_MIN_SCORE) < 1e-9) return 'strict'
  if (Math.abs(v - TEXT_MIN_SCORE) < 1e-9) return 'default'
  return 'custom'
}

/** 档位 → 发给后端的 minScore（default 档 = undefined 不传 → 后端按模型标定兜底） */
export function textMinScoreOf(v: number | undefined | null): number | undefined {
  const lv = textMinLevel(v)
  if (lv === 'loose') return 0
  if (lv === 'strict') return STRICT_TEXT_MIN_SCORE
  if (lv === 'custom') return v as number
  return undefined
}

/**
 * 标签查询入口归一化（task，与 core_types::normalize_tag_query 同口径）：
 * trim 首尾空白 + 内部空白串折叠成 `_`（统一标签词间用下划线，用户爱打空格）。
 * 大小写不在前端折叠——后端 SQL 侧 `COLLATE NOCASE` 处理（走 nocase 索引）。
 */
export function normalizeTagQuery(input: string): string {
  return input.trim().replace(/\s+/g, '_')
}

/** 纯过滤浏览的条件描述（结果区徽标 / 空态文案用） */
export function describeBrowseFilters(tag: string, dirPrefix: string, ext: string): string {
  const parts: string[] = []
  const t = normalizeTagQuery(tag)
  if (t) parts.push(tGlobal('标签「{tag}」', { tag: t }))
  if (dirPrefix) parts.push(tGlobal('目录 {dir}', { dir: dirPrefix }))
  const x = ext.trim().toLowerCase()
  if (x) parts.push(tGlobal('扩展名 {ext}', { ext: x }))
  return parts.join(' · ')
}
