/**
 * E1 图库快照缓存：离开图库页（路由切换）再返回时，
 * 恢复同一过滤条件下的已加载数据与网格滚动位置（不重新拉首页、不跳顶）。
 * 模块级单例（SPA 生命周期内有效，刷新页面自然失效）。
 */
import type { AssetView } from './api'

export interface GallerySnapshot {
  filterKey: string
  items: AssetView[]
  total: number
  scrollTop: number
}

let snapshot: GallerySnapshot | null = null

export function saveGallerySnapshot(s: GallerySnapshot): void {
  snapshot = s
}

export function takeGallerySnapshot(): GallerySnapshot | null {
  return snapshot
}

export function clearGallerySnapshot(): void {
  snapshot = null
}
