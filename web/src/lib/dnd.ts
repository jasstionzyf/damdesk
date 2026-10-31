/**
 * E4 拖拽载荷（模块级单例）。
 *
 * HTML5 DnD 的 dataTransfer 在 dragover 阶段读不到数据（安全限制），
 * 所以真实载荷放这里：dragstart 写入、drop 取走、dragend 清空。
 * dataTransfer 里同步放一份 `text/plain`（assetIds CSV），仅供浏览器原生语义/调试。
 */

export const DND_MIME = 'application/x-dam-asset-ids'

let payload: number[] | null = null

/** dragstart：记录拖拽的 assetId 集合（拖已选项 = 整个选择集；拖未选项 = 单张） */
export function setDragPayload(ids: number[]): void {
  payload = ids.length > 0 ? [...ids] : null
}

/** drop：取走载荷（取完即清空，防止残留状态影响下一次拖拽） */
export function takeDragPayload(): number[] {
  const p = payload
  payload = null
  return p ?? []
}

/** dragend 兜底清理（未发生 drop 时） */
export function clearDragPayload(): void {
  payload = null
}

/** 仅供测试/调试读取 */
export function peekDragPayload(): number[] {
  return payload ? [...payload] : []
}

/** 拖拽目标类型（决定 drop 后走移动 / 打标 / 导出哪条链路） */
export type DragIntent = 'move' | 'tag' | 'export'
