import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { UndoQueue, UNDO_DEFAULT_MS, UNDO_MAX_VISIBLE } from '../src/lib/undoQueue'

/**
 * UI-UNDO 引擎单测（卡 t_2d425292 验收 2）：
 *  - ① undo 超时后请求才发出（窗口期内服务端零请求）
 *  - ② 点「撤销」→ 请求被取消 + 前端状态还原
 *  - ③ 悬停暂停期间绝不提交；恢复后按剩余时间继续
 *  - ④ 超过 3 条：最旧一条当场提交（不静默丢弃，避免界面/服务端分叉）
 */
describe('UndoQueue：可逆操作「延迟提交 + 撤销」', () => {
  beforeEach(() => {
    vi.useFakeTimers()
  })
  afterEach(() => {
    vi.useRealTimers()
  })

  it('① 窗口期内不发请求；10s 到期后才调 onCommit（真删）', async () => {
    const q = new UndoQueue()
    const onCommit = vi.fn()
    const onUndo = vi.fn()
    q.push({ message: '已解绑标签「动物」', onCommit, onUndo })

    // 窗口期内（9.9s）一次请求都不能发出
    await vi.advanceTimersByTimeAsync(UNDO_DEFAULT_MS - 100)
    expect(onCommit).not.toHaveBeenCalled()
    expect(onUndo).not.toHaveBeenCalled()
    expect(q.size()).toBe(1)

    // 到期 = 落库
    await vi.advanceTimersByTimeAsync(100)
    expect(onCommit).toHaveBeenCalledTimes(1)
    expect(onUndo).not.toHaveBeenCalled()
    expect(q.size()).toBe(0)
    expect(q.history()).toEqual([{ id: 1, reason: 'timeout' }])
  })

  it('② 点「撤销」→ onUndo 跑、onCommit 永远不跑（请求被取消）', async () => {
    const q = new UndoQueue()
    const onCommit = vi.fn()
    let tagVisible = false
    const id = q.push({
      message: '已解绑标签「动物」',
      onCommit,
      onUndo: () => {
        tagVisible = true // 还原前端状态（标签回到检查器）
      },
    })

    expect(q.undo(id)).toBe(true)
    expect(tagVisible).toBe(true)
    expect(q.size()).toBe(0)

    // 撤销后再怎么等也不会有延迟提交
    await vi.advanceTimersByTimeAsync(UNDO_DEFAULT_MS * 3)
    expect(onCommit).not.toHaveBeenCalled()
    expect(q.history()).toEqual([{ id, reason: 'undo' }])
  })

  it('③ 悬停暂停：暂停期间不提交，恢复后按剩余窗口继续', async () => {
    const q = new UndoQueue()
    const onCommit = vi.fn()
    const id = q.push({ message: '裁决', onCommit, duration: 10_000 })

    await vi.advanceTimersByTimeAsync(4_000)
    q.pause(id)
    const frozen = q.remaining(id)
    expect(frozen).toBeGreaterThan(5_000)
    expect(frozen).toBeLessThanOrEqual(6_000)

    // 悬停 60s：读 toast 的时候不会被倒计时抢走
    await vi.advanceTimersByTimeAsync(60_000)
    expect(onCommit).not.toHaveBeenCalled()
    expect(q.remaining(id)).toBe(frozen) // 冻结值不变

    q.resume(id)
    await vi.advanceTimersByTimeAsync(frozen)
    expect(onCommit).toHaveBeenCalledTimes(1)
  })

  it(`④ 同时最多 ${UNDO_MAX_VISIBLE} 条：被挤出的最旧一条当场提交（不静默丢失）`, async () => {
    const q = new UndoQueue()
    const commits = [vi.fn(), vi.fn(), vi.fn(), vi.fn()]
    commits.forEach((fn, i) => q.push({ message: `op-${i}`, onCommit: fn }))

    expect(q.size()).toBe(UNDO_MAX_VISIBLE)
    // 第一条被挤出 = 立即落库
    expect(commits[0]).toHaveBeenCalledTimes(1)
    expect(commits[1]).not.toHaveBeenCalled()
    expect(q.items().map((i) => i.message)).toEqual(['op-1', 'op-2', 'op-3'])
    expect(q.history()).toEqual([{ id: 1, reason: 'evicted' }])
  })

  it('⑤ commitAll：离开页面前的兜底提交，返回条数', async () => {
    const q = new UndoQueue()
    const a = vi.fn()
    const b = vi.fn()
    q.push({ message: 'a', onCommit: a })
    q.push({ message: 'b', onCommit: b })
    await expect(q.commitAll()).resolves.toBe(2)
    expect(a).toHaveBeenCalledTimes(1)
    expect(b).toHaveBeenCalledTimes(1)
    expect(q.size()).toBe(0)
  })

  it('⑥ 未知 id 的 undo/commit 是安全空操作（重复点击不重复提交）', async () => {
    const q = new UndoQueue()
    const onCommit = vi.fn()
    const id = q.push({ message: 'x', onCommit })
    expect(q.commit(id)).toBeTruthy()
    await vi.advanceTimersByTimeAsync(0)
    expect(q.undo(id)).toBe(false)
    await q.commit(id)
    expect(onCommit).toHaveBeenCalledTimes(1)
  })

  it('⑦ 提交顺序：onCommit 抛错不会卡住队列，也不影响后续条目', async () => {
    const q = new UndoQueue()
    const bad = vi.fn(() => {
      throw new Error('boom')
    })
    const good = vi.fn()
    q.push({ message: 'bad', onCommit: bad, duration: 1_000 })
    q.push({ message: 'good', onCommit: good, duration: 1_000 })
    await vi.advanceTimersByTimeAsync(1_200)
    expect(bad).toHaveBeenCalledTimes(1)
    expect(good).toHaveBeenCalledTimes(1)
    expect(q.size()).toBe(0)
  })
})
