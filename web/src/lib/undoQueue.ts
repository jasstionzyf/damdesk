/**
 * UI-UNDO：可逆操作的「延迟提交 + 撤销」引擎（纯 TS，无 React / 无 DOM，可单测）。
 *
 * 语义（UI 规范 §9.2.2 红线：可逆操作给撤销，不弹确认）：
 *   1. 用户点「解绑标签 / 去重裁决（移入回收站）」等**可恢复**动作时，**先不改服务端**，
 *      只把动作排进本队列（乐观 UI 已把条目从界面移除），并弹一条带「撤销」的 toast。
 *   2. `duration` 内点「撤销」→ `undo(id)`：清计时器 + 回滚回调，**服务端请求一次都没发**。
 *   3. 计时到点 → `commit(id)`：真正调用 `onCommit()`（网络请求），服务端状态才变。
 *   4. 悬停暂停：`pause(id)` / `resume(id)` 让用户在读 toast 时不会被倒计时抢走。
 *   5. 同时最多 3 条：被挤出的最旧一条**当场提交**（绝不能静默丢弃 —— 界面已移除但服务端
 *      没写，会造成「界面说没了、刷新又回来」的状态分叉）。这不是「又弹一次确认」，
 *      而是「撤销窗口期结束」。
 */
export type UndoHandle = ReturnType<typeof setTimeout>

export interface UndoOpts {
  /** toast 主文案，如「已解绑标签「动物」」 */
  message: string
  /** 副文案：可回滚性 / 作用范围说明 */
  detail?: string
  /** 撤销窗口期（ms），默认 10s */
  duration?: number
  /** 点「撤销」时调用：回滚前端状态（不发请求） */
  onUndo?: () => void
  /** 窗口期结束（超时 / 被挤出 / 显式提交）时调用：真正发请求 */
  onCommit?: () => void | Promise<void>
}

/** 队列对外暴露的只读项（渲染用） */
export interface UndoItem {
  id: number
  message: string
  detail?: string
  duration: number
}

interface Slot {
  item: UndoItem
  onUndo?: () => void
  onCommit?: () => void | Promise<void>
  /** 剩余毫秒（暂停时冻结） */
  remaining: number
  /** 本轮计时起点（Date.now） */
  startedAt: number
  handle: UndoHandle | null
  paused: boolean
}

export const UNDO_MAX_VISIBLE = 3
export const UNDO_DEFAULT_MS = 10_000

export type CommitReason = 'timeout' | 'evicted' | 'manual'

export class UndoQueue {
  private slots = new Map<number, Slot>()
  /** 稳定顺序：旧 → 新 */
  private order: number[] = []
  private seq = 1
  private listeners = new Set<() => void>()
  /** 已提交/已撤销的操作 id（便于测试与「不得既确认又撤销」自查） */
  private settled: { id: number; reason: CommitReason | 'undo' }[] = []

  subscribe(fn: () => void): () => void {
    this.listeners.add(fn)
    return () => {
      this.listeners.delete(fn)
    }
  }

  private emit() {
    for (const fn of this.listeners) fn()
  }

  /** 渲染用快照（旧 → 新） */
  items(): UndoItem[] {
    return this.order.filter((id) => this.slots.has(id)).map((id) => this.slots.get(id)!.item)
  }

  size(): number {
    return this.order.filter((id) => this.slots.has(id)).length
  }

  history(): { id: number; reason: CommitReason | 'undo' }[] {
    return [...this.settled]
  }

  push(opts: UndoOpts): number {
    const id = this.seq++
    const duration = opts.duration ?? UNDO_DEFAULT_MS
    const slot: Slot = {
      item: { id, message: opts.message, detail: opts.detail, duration },
      onUndo: opts.onUndo,
      onCommit: opts.onCommit,
      remaining: duration,
      startedAt: 0,
      handle: null,
      paused: false,
    }
    this.slots.set(id, slot)
    this.order.push(id)
    this.arm(id)
    // 超出上限：最旧的一条当场提交（窗口期被挤掉 = 到期）
    while (this.size() > UNDO_MAX_VISIBLE) {
      const oldest = this.order.find((x) => this.slots.has(x))
      if (oldest === undefined) break
      void this.commit(oldest, 'evicted')
    }
    this.emit()
    return id
  }

  private arm(id: number) {
    const s = this.slots.get(id)
    if (!s) return
    s.startedAt = Date.now()
    s.handle = setTimeout(() => {
      void this.commit(id, 'timeout')
    }, s.remaining)
  }

  private disarm(s: Slot) {
    if (s.handle !== null) {
      clearTimeout(s.handle)
      s.handle = null
    }
  }

  /** 剩余窗口毫秒（暂停时返回冻结值） */
  remaining(id: number): number {
    const s = this.slots.get(id)
    if (!s) return 0
    if (s.paused) return s.remaining
    return Math.max(0, s.remaining - (Date.now() - s.startedAt))
  }

  /** 悬停暂停倒计时（鼠标移入 toast / 键盘聚焦） */
  pause(id: number): void {
    const s = this.slots.get(id)
    if (!s || s.paused) return
    s.remaining = this.remaining(id)
    s.paused = true
    this.disarm(s)
    this.emit()
  }

  /** 移出恢复倒计时（剩余时间接着走，不重置窗口） */
  resume(id: number): void {
    const s = this.slots.get(id)
    if (!s || !s.paused) return
    s.paused = false
    this.arm(id)
    this.emit()
  }

  /** 点「撤销」：清计时器 + 回滚前端状态；服务端请求不会发出 */
  undo(id: number): boolean {
    const s = this.slots.get(id)
    if (!s) return false
    this.close(id)
    this.settled.push({ id, reason: 'undo' })
    this.emit()
    s.onUndo?.()
    return true
  }

  /** 真正提交（发请求）；`reason` 只用于留痕与报告自查 */
  async commit(id: number, reason: CommitReason = 'manual'): Promise<boolean> {
    const s = this.slots.get(id)
    if (!s) return false
    this.close(id)
    this.settled.push({ id, reason })
    this.emit()
    try {
      await s.onCommit?.()
    } catch {
      /* 提交失败由调用方（页面）自己 toast + 回滚；引擎不吞掉页面状态 */
    }
    return true
  }

  /** 强制提交全部（如离开页面/关闭窗口前的兜底），返回提交条数 */
  async commitAll(reason: CommitReason = 'manual'): Promise<number> {
    const ids = this.items().map((i) => i.id)
    for (const id of ids) await this.commit(id, reason)
    return ids.length
  }

  private close(id: number) {
    const s = this.slots.get(id)
    if (!s) return
    this.disarm(s)
    this.slots.delete(id)
    this.order = this.order.filter((x) => x !== id)
  }
}
