/**
 * 搜索 → 图库 的「临时集合」交接（卡 t_dc97b141）。
 *
 * 搜索页只负责查询：批量打标/移动/色标/导出这些动作复用图库页的既有实现，
 * 所以搜索结果以临时集合的形式交给图库（与 E4「找相似」同一套临时集合 UI），
 * 不另写一套批量操作。交接经 localStorage（同 tab 内路由跳转即读即清）。
 */

const KEY = 'dam.pendingSet'

export interface PendingSet {
  /** 集合标题（图库横幅上展示，如「搜索「雪山」的 24 张结果」） */
  label: string
  /** 集合成员（视图顺序） */
  ids: number[]
}

export function stashPendingSet(set: PendingSet): void {
  try {
    localStorage.setItem(KEY, JSON.stringify(set))
  } catch {
    /* 隐私模式/配额满：交接失败不阻塞主流程（用户仍在图库里手动筛选） */
  }
}

export function peekPendingSet(): PendingSet | null {
  try {
    const raw = localStorage.getItem(KEY)
    if (!raw) return null
    const v = JSON.parse(raw) as Partial<PendingSet>
    if (typeof v.label !== 'string' || !Array.isArray(v.ids)) return null
    const ids = v.ids.filter((n): n is number => typeof n === 'number')
    return ids.length > 0 ? { label: v.label, ids } : null
  } catch {
    return null
  }
}

export function clearPendingSet(): void {
  try {
    localStorage.removeItem(KEY)
  } catch {
    /* 忽略：残留的交接数据下次进入图库时按同一规则被覆盖 */
  }
}
