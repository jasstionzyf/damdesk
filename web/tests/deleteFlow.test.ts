/**
 * P1-4 删除流程接线单测（卡 t_bf8f7e95 + 收尾卡 t_bf8f7e95/t_bfe1938c 合流）：
 * 断言的不是「点了看起来有反应」，而是
 *   - 撤销窗口期内**服务端零请求**（不是「先真删再 restore」的两次写）；
 *   - 「撤销」走纯前端回滚（onRestore），不调任何写接口；
 *   - 窗口期到点才调 libraryDelete 真正落库，失败要回滚 + 不静默。
 *
 * 用假 api + 假 toast（记录 undo 的 onUndo/onCommit 供测试手动触发），node 环境无需 jsdom。
 */
import { describe, expect, it, vi } from 'vitest'
import { DELETE_UNDO_MS, deleteWithUndo, type SoftDeleteApi, type ToastWithUndo } from '../src/lib/deleteFlow'
import type { ToastAction, ToastKind } from '../src/lib/toast'
import type { UndoOpts } from '../src/lib/undoQueue'

function fakeToast() {
  const calls: { kind: ToastKind; title: string; detail?: string; action?: ToastAction }[] = []
  const undos: UndoOpts[] = []
  const fn = ((kind: ToastKind, title: string, detail?: string, action?: ToastAction) => {
    calls.push({ kind, title, detail, action })
  }) as ToastWithUndo
  fn.undo = (opts: UndoOpts) => {
    undos.push(opts)
    return undos.length
  }
  return { fn, calls, undos }
}

function fakeApi(over: Partial<SoftDeleteApi> = {}) {
  const del = vi.fn(async (ids: number[]) => ({ deleted: ids.length }))
  return { api: { libraryDelete: del, ...over } as SoftDeleteApi, del }
}

function fakeHooks() {
  const removed: number[][] = []
  const restored: number[][] = []
  const committed: { deleted: number }[] = []
  const rollbacks: number[] = []
  return {
    hooks: {
      onRemove: (ids: number[]) => void removed.push(ids),
      onRestore: (ids: number[]) => void restored.push(ids),
      onCommitted: (r: { deleted: number }) => void committed.push(r),
      onRollback: () => void rollbacks.push(1),
    },
    removed,
    restored,
    committed,
    rollbacks,
  }
}

describe('deleteWithUndo：延迟提交 + 撤销（窗口期零请求）', () => {
  it('非空选择 → 只乐观摘除 + 弹撤销 toast，服务端一次请求都没发', () => {
    const { api, del } = fakeApi()
    const t = fakeToast()
    const h = fakeHooks()

    const r = deleteWithUndo(api, [11, 12, 13], t.fn, h.hooks)
    expect(r.status).toBe('queued')
    expect(h.removed).toEqual([[11, 12, 13]])
    expect(del).not.toHaveBeenCalled() // 关键：窗口期零请求
    expect(t.undos).toHaveLength(1)
    expect(t.undos[0].message).toBe('已移入回收站 3 张')
    expect(t.undos[0].detail).toContain('10 秒内可撤销')
    expect(t.undos[0].duration).toBe(DELETE_UNDO_MS)
  })

  it('窗口期内点撤销 → 纯前端回滚，仍然零请求', () => {
    const { api, del } = fakeApi()
    const t = fakeToast()
    const h = fakeHooks()

    deleteWithUndo(api, [21, 22], t.fn, h.hooks)
    t.undos[0].onUndo?.()
    expect(h.restored).toEqual([[21, 22]])
    expect(del).not.toHaveBeenCalled()
    expect(t.calls).toHaveLength(0) // 撤销不需要额外成功 toast（toast 自己收起来了）
  })

  it('窗口期到点提交 → 才调 libraryDelete，并回报 deleted', async () => {
    const { api, del } = fakeApi()
    const t = fakeToast()
    const h = fakeHooks()

    deleteWithUndo(api, [31, 32], t.fn, h.hooks)
    await t.undos[0].onCommit?.()
    expect(del).toHaveBeenCalledWith([31, 32])
    expect(h.committed).toEqual([{ deleted: 2 }])
    expect(h.restored).toHaveLength(0)
  })

  it('落库失败 → error toast 带后端文案 + 回滚 + 回读，不静默不假成功', async () => {
    const api: SoftDeleteApi = {
      libraryDelete: async () => {
        throw new Error('db is locked')
      },
    }
    const t = fakeToast()
    const h = fakeHooks()

    deleteWithUndo(api, [7], t.fn, h.hooks)
    await t.undos[0].onCommit?.()
    expect(h.restored).toEqual([[7]])
    expect(h.rollbacks).toHaveLength(1)
    expect(h.committed).toHaveLength(0)
    expect(t.calls.at(-1)!.kind).toBe('error')
    expect(t.calls.at(-1)!.title).toBe('删除失败，已回滚')
    expect(t.calls.at(-1)!.detail).toBe('db is locked')
  })

  it('空选择 → 不摘除、不进队列、只给引导文案（防误触）', () => {
    const { api, del } = fakeApi()
    const t = fakeToast()
    const h = fakeHooks()

    const r = deleteWithUndo(api, [], t.fn, h.hooks)
    expect(r.status).toBe('empty')
    expect(h.removed).toHaveLength(0)
    expect(t.undos).toHaveLength(0)
    expect(del).not.toHaveBeenCalled()
    expect(t.calls.at(-1)!.kind).toBe('warning')
    expect(t.calls.at(-1)!.title).toBe('没有可删除的图片')
  })
})
