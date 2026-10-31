/**
 * 导出目标路径联动（task 回归修复）：
 * dir↔zip 模式切换时目标输入框同步换形——
 *   目录 → zip：/<prefix> → /<prefix>.zip（先去掉尾部分隔符）
 *   zip → 目录：/<prefix>.zip → /<prefix>
 * 空输入原样返回（占位符提示接管）。
 */
import type { ExportFilterPayload } from './api'

export function switchExportDest(mode: 'dir' | 'zip', dest: string): string {
  const cur = dest.trim()
  if (!cur) return dest
  if (mode === 'zip') {
    const base = cur.replace(/\/+$/, '')
    return /\.zip$/i.test(base) ? base : `${base}.zip`
  }
  return cur.replace(/\.zip$/i, '')
}

// ---------------------------------------------------------------------------------------
// 「当前筛选结果」范围上下文（卡 task：导出收编为全局 Dialog，页面 /export 撤销）
// ---------------------------------------------------------------------------------------

/**
 * 打开导出 Dialog 的页面（图库 / 搜索 browse）随手寄存的「当前筛选结果」上下文：
 *  - `label`：来源口径（如「图库当前筛选」），Dialog 范围行展示用；
 *  - `filter`：与 `/api/library/assets` 同形的查询条件 —— **计数与导出必须用同一份对象**
 *    （DT4 教训：预览和导出各算一份就会出现「预览 0 张 / 实际整库」的自相矛盾读数）。
 *
 * 载体选 sessionStorage：F5 后「当前筛选结果」选项仍在（ids 本来就在 URL 里，两边都能还原），
 * 关标签页即弃、不跨设备漫游。寄存失败（隐私模式等）只是少这一个范围选项，不阻塞选择集导出。
 */
export interface ExportFilterScope {
  label: string
  filter: ExportFilterPayload
}

const FILTER_SCOPE_KEY = 'dam.export.filterScope'

/** 寄存 / 清空（传 null）「当前筛选结果」上下文；打开导出 Dialog 的入口顺手调用 */
export function stashExportFilterScope(scope: ExportFilterScope | null): void {
  try {
    if (!scope) sessionStorage.removeItem(FILTER_SCOPE_KEY)
    else sessionStorage.setItem(FILTER_SCOPE_KEY, JSON.stringify(scope))
  } catch {
    // sessionStorage 不可用：静默降级为「只有选择集范围」
  }
}

/** 读回上下文；没有或脏数据 → null（Dialog 隐藏「当前筛选结果」选项，不猜） */
export function loadExportFilterScope(): ExportFilterScope | null {
  try {
    const raw = sessionStorage.getItem(FILTER_SCOPE_KEY)
    if (!raw) return null
    const v = JSON.parse(raw) as ExportFilterScope | null
    if (!v || typeof v !== 'object' || typeof v.label !== 'string' || typeof v.filter !== 'object') {
      return null
    }
    return v
  } catch {
    return null
  }
}
