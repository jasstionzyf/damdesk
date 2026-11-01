/**
 * UX: 图库筛选/选择持久化 —— F5 刷新不丢。
 * - 筛选条件（目录/标签/未打标/视图 + 文件名快速筛选）→ localStorage（跨会话保留）
 * - 选择集 → sessionStorage（至少会话内保留，关标签页失效）
 * 芯片「一键清除」走 applyFilter(defaultFilter)，经持久化 effect 自动落盘，无需额外入口。
 * 读写全部 try/catch：存储不可用（隐私模式/配额满）时静默退化为内存态。
 */
import type { LibraryFilter } from '../components/LibraryTree'
import type { SelState } from './selection'

const FILTER_KEY = 'dam.gallery.filter'
const QUERY_KEY = 'dam.gallery.query'
const SEL_KEY = 'dam.gallery.sel'

/** 恢复筛选：逐字段校验类型（脏数据/旧版本 key → null 回退默认，不抛错） */
export function loadPersistedFilter(): LibraryFilter | null {
  try {
    const raw = localStorage.getItem(FILTER_KEY)
    if (!raw) return null
    const f = JSON.parse(raw) as LibraryFilter
    if (f === null || typeof f !== 'object') return null
    if (f.view !== 'all' && f.view !== 'recent') return null
    if (f.untagged !== undefined && typeof f.untagged !== 'boolean') return null
    if (f.dir !== undefined && typeof f.dir !== 'string') return null
    if (f.tag !== undefined && typeof f.tag !== 'string') return null
    // DECouple-FE：`compliance`（判定筛选）与 `minRating`（KI-11 前的评分下限）都**不再是图库维度**。
    // 旧档里可能还存着它们 —— 一律丢弃，不复活成任何界面状态（否则旧库打开就恒 0 张，
    // 用户以为图丢了）。图库只认资产域条件（目录/标签/未打标/失联/无法预览/EXIF）。
    const { compliance: _drop, minRating: _dropToo, ...rest } = f as unknown as Record<string, unknown>
    void _drop
    void _dropToo
    return rest as unknown as LibraryFilter
  } catch {
    return null
  }
}

export function savePersistedFilter(f: LibraryFilter): void {
  try {
    localStorage.setItem(FILTER_KEY, JSON.stringify(f))
  } catch {
    /* ignore */
  }
}

export function loadPersistedQuery(): string {
  try {
    return localStorage.getItem(QUERY_KEY) ?? ''
  } catch {
    return ''
  }
}

export function savePersistedQuery(q: string): void {
  try {
    if (q) localStorage.setItem(QUERY_KEY, q)
    else localStorage.removeItem(QUERY_KEY)
  } catch {
    /* ignore */
  }
}

/** 选择集：校验 ids 为整数数组（assetId 引用可跨页保留，批量操作按 ids 直达后端） */
export function loadPersistedSelection(): SelState | null {
  try {
    const raw = sessionStorage.getItem(SEL_KEY)
    if (!raw) return null
    const s = JSON.parse(raw) as SelState
    if (!Array.isArray(s.ids) || !s.ids.every((n) => Number.isInteger(n) && n > 0)) return null
    return {
      ids: s.ids,
      anchor: typeof s.anchor === 'number' && s.anchor > 0 ? s.anchor : null,
      active: typeof s.active === 'number' && s.active > 0 ? s.active : null,
    }
  } catch {
    return null
  }
}

export function savePersistedSelection(s: SelState): void {
  try {
    if (s.ids.length === 0) sessionStorage.removeItem(SEL_KEY)
    else sessionStorage.setItem(SEL_KEY, JSON.stringify(s))
  } catch {
    /* ignore */
  }
}
