/**
 * E2 选择模型（纯函数，无 React / 无 DOM —— 可被 node 直接跑测试）。
 *
 * 语义（Lightroom 惯例）：
 * - 单击 / ←→ ：单选（active = anchor = id）
 * - cmd|ctrl+单击：加选 / 取消加选（active = anchor = id）
 * - shift+单击 / shift+←→：从 anchor 到目标的范围选（anchor 保持，active 移动）
 * - cmd|ctrl+A：全选当前视图
 * - Esc：清空
 *
 * 不变式：`ids` 顺序恒等于「视图顺序」（传入的 order），范围选/全选/加选后仍成立。
 */

export interface SelState {
  /** 选中集合（按视图顺序） */
  ids: number[]
  /** 范围选锚点（shift 选起点） */
  anchor: number | null
  /** 当前游标（网格高亮 / Lightbox 显示的那张） */
  active: number | null
}

export const EMPTY: SelState = { ids: [], anchor: null, active: null }

/** 视图顺序（显示序 assetId 数组）里的下标；不存在 → -1 */
function indexOf(order: number[], id: number | null): number {
  if (id === null) return -1
  return order.indexOf(id)
}

function clamp(v: number, lo: number, hi: number): number {
  return v < lo ? lo : v > hi ? hi : v
}

/** 按视图顺序重排（不在 order 里的 id 排在最后，保序） */
export function sortByOrder(order: number[], ids: number[]): number[] {
  const known: number[] = []
  const rest: number[] = []
  for (const id of ids) {
    if (order.includes(id)) known.push(id)
    else rest.push(id)
  }
  known.sort((a, b) => order.indexOf(a) - order.indexOf(b))
  return [...known, ...rest]
}

export function isOn(state: SelState, id: number | null): boolean {
  return id !== null && state.ids.includes(id)
}

/** 单选 */
export function single(id: number): SelState {
  return { ids: [id], anchor: id, active: id }
}

/** 加选 / 取消加选（视图顺序保持） */
export function toggle(state: SelState, order: number[], id: number): SelState {
  const next = state.ids.includes(id) ? state.ids.filter((x) => x !== id) : [...state.ids, id]
  return { ids: sortByOrder(order, next), anchor: id, active: id }
}

/** 范围选：anchor（缺失则用目标自身）→ 目标，按视图顺序取整段 */
export function range(order: number[], state: SelState, id: number): SelState {
  const fromIdx = indexOf(order, state.anchor)
  const toIdx = indexOf(order, id)
  if (toIdx < 0) return single(id)
  const from = fromIdx >= 0 ? fromIdx : toIdx
  const [lo, hi] = from <= toIdx ? [from, toIdx] : [toIdx, from]
  return { ids: order.slice(lo, hi + 1), anchor: order[from], active: id }
}

/** 全选当前视图 */
export function all(order: number[]): SelState {
  if (order.length === 0) return EMPTY
  return { ids: [...order], anchor: order[0], active: order[order.length - 1] }
}

export function clear(): SelState {
  return EMPTY
}

/**
 * ←→ 移动游标：delta=±1 单选移动；extend=true 时保持 anchor 做范围扩展。
 * 到达边界时返回原状态（不循环）。
 */
export function move(state: SelState, order: number[], delta: number, extend = false): SelState {
  if (order.length === 0) return EMPTY
  const cur = indexOf(order, state.active)
  const base = cur >= 0 ? cur : 0
  const target = clamp(base + delta, 0, order.length - 1)
  if (cur >= 0 && target === base) return state
  const id = order[target]
  if (extend) {
    const anchor = indexOf(order, state.anchor) >= 0 ? state.anchor : order[base]
    return range(order, { ...state, anchor }, id)
  }
  return single(id)
}

/** Lightbox 翻页下标（夹取，不循环） */
export function navIndex(len: number, cur: number, delta: number): number {
  if (len <= 0) return 0
  return clamp(cur + delta, 0, len - 1)
}

/** 快速筛选（工具条搜索框）：文件名 / 标题 / 描述 子串匹配，大小写不敏感 */
export function filterByQuery<
  T extends { fileName: string; titleCh?: string | null; titleEn?: string | null; description?: string | null },
>(items: T[], q: string): T[] {
  const needle = q.trim().toLowerCase()
  if (!needle) return items
  return items.filter((a) =>
    [a.fileName, a.titleCh, a.titleEn, a.description]
      .filter((s): s is string => typeof s === 'string' && s.length > 0)
      .some((s) => s.toLowerCase().includes(needle)),
  )
}

/** 打分/拒选目标：有选择集用选择集，否则用当前游标（键盘 1-5 / X 无选择时作用于游标） */
export function targetIds(state: SelState): number[] {
  if (state.ids.length > 0) return state.ids
  return state.active !== null ? [state.active] : []
}
