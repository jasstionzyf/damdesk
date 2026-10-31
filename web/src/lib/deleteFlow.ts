/**
 * P1-4 × UI-UNDO 合流：图库「删除 → 撤销」流程（纯函数，可在 node 环境单测）。
 *
 * 为什么抽出来：删除是破坏性动作，正确性不能只靠「点了看起来有反应」验收。
 * 这里把「调哪个接口 / toast 文案 / 撤销按钮接了谁 / 失败怎么回滚」固化成可断言的函数，
 * Gallery 只负责搬运 UI 状态（列表摘除、选择集清空、侧栏刷新）。
 *
 * 语义（与 svc-library 一致）：软删 deleted=1，源文件不动，restore 可恢复。
 *
 * 与 UI-UNDO（卡 task，lib/undoQueue.ts）统一后**不再**走「先真删再 restore」的两次写：
 *   1. 用户确认后，前端先乐观摘除条目 + 弹「撤销」toast，**窗口期服务端零请求**；
 *   2. 窗口期内点撤销 → 纯前端回滚（一次请求都不发）；
 *   3. 窗口期到点 / 被挤出 / 显式提交 → 才调 libraryDelete 真正落库；
 *   4. 落库失败 → error toast + 前端回滚 + 回读，不静默、不假成功。
 */
import type { ToastAction, ToastKind } from './toast'
import type { UndoOpts } from './undoQueue'
import { tGlobal } from '../i18n'

/** 只依赖这一个接口（Gallery 传 api，测试传假实现） */
export interface SoftDeleteApi {
  libraryDelete: (ids: number[]) => Promise<{ deleted: number }>
}

/** toast 函数 + 撤销扩展（与 lib/toast.tsx 的 ToastFn 对齐，测试可传假实现） */
export type ToastWithUndo = ((
  kind: ToastKind,
  title: string,
  detail?: string,
  action?: ToastAction,
) => void) & {
  undo: (opts: UndoOpts) => number
}

/** Gallery 侧的 UI 状态搬运（回调里只动本地状态 / 回读，不发写请求） */
export interface DeleteHooks {
  /** 乐观摘除：列表里移除这批图、总数递减、清空选择集 */
  onRemove: (ids: number[]) => void
  /** 撤销：纯前端回滚到摘除前的快照 */
  onRestore: (ids: number[]) => void
  /** 真正落库成功后：刷新侧栏计数 / 回读分页 */
  onCommitted?: (r: { deleted: number }) => void
  /** 落库失败：回读对齐服务端真值 */
  onRollback?: () => void
}

export type QueueDeleteResult =
  | { status: 'queued'; ids: number[]; toastId: number }
  | { status: 'empty' }

const errText = (e: unknown) => (e instanceof Error ? e.message : String(e))

/** 撤销窗口期（ms）：与 undoQueue 默认一致 */
export const DELETE_UNDO_MS = 10_000

/**
 * 删除入口：暂存到撤销队列（窗口期零请求），到点才真删。
 * 调用方**先**弹确认对话框（lib/confirm.tsx useConfirm），确认后再调本函数。
 */
export function deleteWithUndo(
  api: SoftDeleteApi,
  ids: number[],
  toast: ToastWithUndo,
  hooks: DeleteHooks,
): QueueDeleteResult {
  if (ids.length === 0) {
    toast(
      'warning',
      tGlobal('没有可删除的图片'),
      tGlobal('先选中图片：单击 / Shift 范围选 / Cmd(Ctrl)+A 全选，再按 Delete'),
    )
    return { status: 'empty' }
  }

  const batch = [...ids]
  hooks.onRemove(batch)

  const toastId = toast.undo({
    message: tGlobal('已移入回收站 {n} 张', { n: batch.length }),
    detail: tGlobal(
      '10 秒内可撤销（撤销不发任何请求）；超时才真正落库，源文件仍在磁盘，之后可在「回收站」恢复',
    ),
    duration: DELETE_UNDO_MS,
    onUndo: () => {
      // 纯前端回滚：服务端一次请求都没发，所以「撤销」不需要 restore 接口
      hooks.onRestore(batch)
    },
    onCommit: async () => {
      try {
        const r = await api.libraryDelete(batch)
        hooks.onCommitted?.(r)
      } catch (e) {
        toast('error', tGlobal('删除失败，已回滚'), errText(e))
        hooks.onRestore(batch)
        hooks.onRollback?.()
      }
    },
  })

  return { status: 'queued', ids: batch, toastId }
}
