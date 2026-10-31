import React, {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from 'react'
import { UndoQueue, UNDO_MAX_VISIBLE, type UndoItem, type UndoOpts } from './undoQueue'
import { tGlobal, useI18n } from '../i18n'

export type ToastKind = 'info' | 'success' | 'error' | 'warning'

export interface ToastAction {
  label: string
  onClick: () => void
}

export interface ToastItem {
  id: number
  kind: ToastKind
  title: string
  detail?: string
  action?: ToastAction
}

/** 普通 toast：`toast(kind, title, detail?, action?)`；撤销 toast：`toast.undo({...})` */
export interface ToastFn {
  (kind: ToastKind, title: string, detail?: string, action?: ToastAction): void
  /**
   * 可逆操作专用：**立即**弹一条带「撤销」的 toast，窗口期内不发任何请求；
   * 撤销 → 只回滚前端；超时 → 调 `onCommit()` 真正落库。
   * 返回队列 id（取消/测试用）。
   */
  undo: (opts: UndoOpts) => number
}

interface ToastApi {
  toast: ToastFn
  dismiss: (id: number) => void
}

const ToastCtx = createContext<ToastApi | null>(null)

let nextId = 1

const KIND_STYLE: Record<ToastKind, string> = {
  info: 'border-line',
  success: 'border-emerald-600/60',
  error: 'border-red-600/60',
  warning: 'border-amber-600/60',
}

const KIND_DOT: Record<ToastKind, string> = {
  info: 'bg-accent',
  success: 'bg-emerald-500',
  error: 'bg-red-500',
  warning: 'bg-amber-500',
}

const KIND_ICON: Record<ToastKind, string> = {
  info: 'ℹ',
  success: '✓',
  error: '✕',
  warning: '!',
}

/** 普通 toast 同时最多显示 3 条（撤销 toast 另有 3 条上限，见 UndoQueue） */
const MAX_TOASTS = 3

/** 撤销 toast：悬停/聚焦暂停倒计时，点「撤销」才回滚 */
function UndoCard({
  item,
  queue,
}: {
  item: UndoItem
  queue: UndoQueue
}) {
  const { t } = useI18n()
  const [paused, setPaused] = useState(false)
  const [left, setLeft] = useState(() => Math.ceil(queue.remaining(item.id) / 1000))

  useEffect(() => {
    const t = window.setInterval(() => {
      setLeft(Math.ceil(queue.remaining(item.id) / 1000))
    }, 250)
    return () => window.clearInterval(t)
  }, [queue, item.id])

  const pause = () => {
    queue.pause(item.id)
    setPaused(true)
  }
  const resume = () => {
    queue.resume(item.id)
    setPaused(false)
  }

  return (
    <div
      data-testid="toast-undo"
      data-undo-id={item.id}
      data-paused={paused ? '1' : '0'}
      className="pointer-events-auto flex items-start gap-2 rounded-md border border-line bg-panel-2 px-3 py-2.5 shadow-lg"
      role="status"
      onMouseEnter={pause}
      onMouseLeave={resume}
      onFocus={pause}
      onBlur={resume}
    >
      <span className="mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center rounded-full bg-ink-dim text-[10px] font-bold text-bg">
        ↺
      </span>
      <div className="min-w-0 flex-1">
        <p className="text-sm leading-5">{item.message}</p>
        {item.detail && <p className="mt-0.5 text-xs leading-4 text-ink-dim">{item.detail}</p>}
        <button
          type="button"
          data-testid="undo-btn"
          className="mt-1.5 rounded border border-accent/60 px-2 py-0.5 text-xs font-medium text-accent hover:bg-accent-soft"
          onClick={() => queue.undo(item.id)}
          title={paused ? t('倒计时已暂停（悬停/聚焦中）') : t('撤销这次操作，服务端不会写入')}
        >
          {paused ? t('撤销（已暂停 · 还剩 {n}s）', { n: left }) : t('撤销（还剩 {n}s）', { n: left })}
        </button>
      </div>
      <button
        type="button"
        aria-label={t('关闭并立即生效')}
        title={t('关闭 = 立即生效（不等倒计时）')}
        className="shrink-0 rounded p-0.5 text-ink-dim hover:bg-panel hover:text-ink"
        onClick={() => {
          void queue.commit(item.id, 'manual')
        }}
      >
        ✕
      </button>
    </div>
  )
}

export function ToastProvider({ children }: { children: React.ReactNode }) {
  const [items, setItems] = useState<ToastItem[]>([])
  // 撤销队列（单例/Provider）：Engine 持有计时器，React 只订阅渲染
  const queueRef = useRef<UndoQueue | null>(null)
  if (queueRef.current === null) queueRef.current = new UndoQueue()
  const queue = queueRef.current
  const [undoItems, setUndoItems] = useState<UndoItem[]>([])

  useEffect(() => {
    const sync = () => setUndoItems(queue.items())
    sync()
    return queue.subscribe(sync)
  }, [queue])

  const dismiss = useCallback((id: number) => {
    setItems((prev) => prev.filter((t) => t.id !== id))
  }, [])

  const toast = useMemo<ToastFn>(() => {
    const fn = ((kind: ToastKind, title: string, detail?: string, action?: ToastAction) => {
      const id = nextId++
      setItems((prev) => [...prev, { id, kind, title, detail, action }].slice(-MAX_TOASTS))
      const ttl = kind === 'error' ? 10000 : 4500
      window.setTimeout(() => dismiss(id), ttl)
    }) as ToastFn
    fn.undo = (opts: UndoOpts) => queue.push(opts)
    return fn
  }, [dismiss, queue])

  return (
    <ToastCtx.Provider value={{ toast, dismiss }}>
      {children}
      {/* toast 栈：右下角，不遮挡任务中心抽屉 */}
      <div className="pointer-events-none fixed right-4 bottom-4 z-50 flex w-80 flex-col gap-2">
        {undoItems.map((t) => (
          <UndoCard key={`undo-${t.id}`} item={t} queue={queue} />
        ))}
        {items.map((t) => (
          <div
            key={t.id}
            className={`pointer-events-auto flex items-start gap-2 rounded-md border bg-panel-2 px-3 py-2.5 shadow-lg ${KIND_STYLE[t.kind]}`}
            role="status"
          >
            <span
              className={`mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center rounded-full text-[10px] font-bold text-bg ${KIND_DOT[t.kind]}`}
            >
              {KIND_ICON[t.kind]}
            </span>
            <div className="min-w-0 flex-1">
              <p className="text-sm leading-5">{t.title}</p>
              {t.detail && <p className="mt-0.5 text-xs leading-4 text-ink-dim">{t.detail}</p>}
              {t.action && (
                <button
                  type="button"
                  className="mt-1.5 rounded border border-line px-2 py-0.5 text-xs text-accent hover:bg-panel"
                  onClick={() => {
                    t.action!.onClick()
                    dismiss(t.id)
                  }}
                >
                  {t.action.label}
                </button>
              )}
            </div>
            <button
              type="button"
              aria-label={tGlobal('关闭')}
              className="shrink-0 rounded p-0.5 text-ink-dim hover:bg-panel hover:text-ink"
              onClick={() => dismiss(t.id)}
            >
              ✕
            </button>
          </div>
        ))}
      </div>
    </ToastCtx.Provider>
  )
}

export function useToast(): ToastApi {
  const ctx = useContext(ToastCtx)
  if (!ctx) throw new Error('useToast must be used within ToastProvider')
  return ctx
}

/** 撤销 toast 上限（渲染/测试一致） */
export const UNDO_TOAST_MAX = UNDO_MAX_VISIBLE
