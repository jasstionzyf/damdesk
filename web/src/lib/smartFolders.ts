/**
 * E3 — 智能文件夹（保存搜索）前端支撑：
 * 类型 / 纯函数（条件摘要）/ 最近命令（命令面板排序）/ 轻量事件总线。
 * API 客户端在 lib/api.ts（smartFolders*），本文件不放网络代码。
 */
import { tGlobal } from '../i18n'

export interface SmartFolder {
  id: number
  name: string
  /** AssetQuery 形态 JSON 字符串（后端白名单归一化） */
  query: string
  createdAt: number
  updatedAt: number
}

/** 保存条件（与 AssetQuery 的可保存子集一致；limit/offset 属分页不保存。
 *  KI-11：评分下限不再可保存 —— 旧档里的评分条件仍能被 parseQuery 读出（数据兼容），
 *  但界面不展示、编辑保存后自然消失。 */
export interface FolderQuery {
  tag?: string
  colorLabel?: string
  dirPrefix?: string
  untagged?: boolean
  mediaType?: string
  ext?: string
  missing?: number
}

export const COLOR_LABELS: { value: string; label: string; dot: string }[] = [
  { value: 'red', label: '红色', dot: '#ef4444' },
  { value: 'orange', label: '橙色', dot: '#f97316' },
  { value: 'yellow', label: '黄色', dot: '#eab308' },
  { value: 'green', label: '绿色', dot: '#22c55e' },
  { value: 'blue', label: '蓝色', dot: '#3b82f6' },
  { value: 'purple', label: '紫色', dot: '#a855f7' },
]

export function colorLabelInfo(value: string): { label: string; dot: string } {
  // i18n-D：颜色名按当前 locale 取词；未知值原样回显
  const hit = COLOR_LABELS.find((c) => c.value === value)
  return hit ? { label: tGlobal(hit.label), dot: hit.dot } : { label: value, dot: '#888' }
}

export function parseQuery(q: string): FolderQuery {
  try {
    const v = JSON.parse(q) as FolderQuery
    return typeof v === 'object' && v !== null ? v : {}
  } catch {
    return {}
  }
}

/** 条件 → 人话摘要（侧栏 tooltip / 查看页头部 / 命令面板 keywords 共用） */
export function querySummary(f: FolderQuery): string {
  const parts: string[] = []
  if (f.tag) parts.push(tGlobal('标签 {tag}', { tag: f.tag }))
  if (f.colorLabel) parts.push(colorLabelInfo(f.colorLabel).label)
  if (f.dirPrefix) {
    const segs = f.dirPrefix.split(/[\\/]/).filter(Boolean)
    parts.push(tGlobal('目录 {dir}', { dir: segs[segs.length - 1] ?? f.dirPrefix }))
  }
  if (f.untagged) parts.push(tGlobal('未打标'))
  if (f.mediaType) parts.push(tGlobal('类型 {type}', { type: f.mediaType }))
  if (f.ext) parts.push(`.${f.ext}`)
  if (f.missing === 1) parts.push(tGlobal('仅失联'))
  return parts.join(' · ')
}

// ---- 最近命令（命令面板「最近使用」排序） ----

const RECENT_KEY = 'dam.cmdk.recent.v1'
const RECENT_CAP = 12

export function recentIds(): string[] {
  try {
    const v = JSON.parse(localStorage.getItem(RECENT_KEY) ?? '[]') as unknown
    return Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : []
  } catch {
    return []
  }
}

export function pushRecent(id: string): void {
  const next = [id, ...recentIds().filter((x) => x !== id)].slice(0, RECENT_CAP)
  localStorage.setItem(RECENT_KEY, JSON.stringify(next))
}

// ---- 保存时的命中数（KI-16 #7 计数对账；不改 /api/smart-folders 契约） ----
//
// 契约里没有 count 字段（边界：不改后端契约），所以「保存时命中多少张」记在本地：
// 保存/改条件时用**与图库同一口径**的查询取一次 total 存下来，图库应用条件后再比一次 ——
// 不一致就在芯片上标「条件已变」（用户改过图库/条件本身已过期，而不是数据显示错误）。

const COUNTS_KEY = 'dam.sf.counts.v1'

function readCounts(): Record<string, number> {
  try {
    const v = JSON.parse(localStorage.getItem(COUNTS_KEY) ?? '{}') as unknown
    if (typeof v !== 'object' || v === null) return {}
    const out: Record<string, number> = {}
    for (const [k, n] of Object.entries(v as Record<string, unknown>)) {
      if (typeof n === 'number' && Number.isFinite(n)) out[k] = n
    }
    return out
  } catch {
    return {}
  }
}

export function recordFolderCount(id: number, count: number): void {
  if (!Number.isFinite(id) || id <= 0 || !Number.isFinite(count)) return
  try {
    const all = readCounts()
    all[String(id)] = Math.max(0, Math.trunc(count))
    localStorage.setItem(COUNTS_KEY, JSON.stringify(all))
  } catch {
    /* 存储不可用：不阻塞保存，只是少了「条件已变」提示 */
  }
}

export function folderCountOf(id: number): number | null {
  if (!Number.isFinite(id) || id <= 0) return null
  const n = readCounts()[String(id)]
  return typeof n === 'number' ? n : null
}

export function forgetFolderCount(id: number): void {
  try {
    const all = readCounts()
    delete all[String(id)]
    localStorage.setItem(COUNTS_KEY, JSON.stringify(all))
  } catch {
    /* 存储不可用：静默 */
  }
}

// ---- 事件总线：智能文件夹增删改后侧栏/面板自刷新 ----

export const SF_EVENT = 'dam:smart-folders-changed'

export function notifyFoldersChanged(): void {
  window.dispatchEvent(new Event(SF_EVENT))
}

// ---- 图库视图切换交接（命令面板 → Gallery 挂载时消费） ----
// 注意 React StrictMode dev 双挂载：初始化只许「窥视」不许取走，
// 由 Gallery 挂载后的延时清理兜底，避免第一次挂载把交接值消费掉。

export type StashedView = 'recent' | 'all'

export function stashView(v: StashedView): void {
  localStorage.setItem('dam.pendingView', v)
}

export function peekStashedView(): StashedView | null {
  const v = localStorage.getItem('dam.pendingView')
  return v === 'recent' || v === 'all' ? v : null
}

export function clearStashedView(): void {
  localStorage.removeItem('dam.pendingView')
}
