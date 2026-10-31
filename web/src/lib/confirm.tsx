import React, { createContext, useCallback, useContext, useEffect, useRef, useState } from 'react'
import { tGlobal } from '../i18n'

/**
 * UI-UNDO：**不可逆/高成本**操作的统一确认（可逆操作走 toast.undo，不弹这个）。
 *
 * 红线（UI 规范 §9.2.2）：
 *   - 主按钮文案必须是「动词 + 真实数量」（如「移动 3 张」），**禁止写「全部」**；
 *   - 数量由调用方从真实选择集/查询结果算出来传进来，不由本对话框猜；
 *   - 默认焦点在「取消」，Esc = 取消，点遮罩 = 取消；
 *   - 必须给出「可回滚性」说明（能不能撤回、怎么回滚）。
 *
 * 用法：
 *   const confirm = useConfirm()
 *   if (!(await confirm({ title, body, impacts, recoverable, confirmLabel: verbCount('移动', ids.length) }))) return
 */

export interface ConfirmOptions {
  title: string
  /** 一句话说明这次操作会做什么 */
  body?: string
  /** 影响明细（来自真实查询/选择集，逐条人话） */
  impacts?: string[]
  /** 可回滚性说明，如「可反向移动回原目录」/「不可撤销」 */
  recoverable?: string
  /** 主按钮：动词 + 数量（用 verbCount() 生成） */
  confirmLabel: string
  cancelLabel?: string
  /** 危险操作（红按钮） */
  danger?: boolean
}

export interface ConfirmApi {
  confirm: (opts: ConfirmOptions) => Promise<boolean>
}

/** 「动词 + 真实数量」文案（唯一入口，测试固定：不得出现「全部」）。
 *  i18n-D：unit 默认值每次调用经 tGlobal 取当前 locale 译文。 */
export function verbCount(verb: string, n: number, unit = tGlobal('张')): string {
  return `${verb} ${n} ${unit}`
}

/** 影响条数行（无条数时返回 null，避免出现「影响：0 项」这种噪音） */
export function impactLine(label: string, n: number, unit = tGlobal('项')): string | null {
  if (!Number.isFinite(n) || n <= 0) return null
  // 分隔符也要按语言取（en 用 ': '；中文全角冒号在英文界面属残留中文标点）
  return `${label}${tGlobal('：')}${n} ${unit}`
}

const ConfirmCtx = createContext<ConfirmApi | null>(null)

interface Pending {
  opts: ConfirmOptions
  resolve: (ok: boolean) => void
}

/** 纯展示组件（无 Provider 依赖，便于 SSR 单测断言文案/焦点） */
export function ConfirmDialog({
  opts,
  onCancel,
  onConfirm,
}: {
  opts: ConfirmOptions
  onCancel: () => void
  onConfirm: () => void
}) {
  const cancelRef = useRef<HTMLButtonElement | null>(null)
  useEffect(() => {
    // 默认焦点在「取消」——误回车不会触发不可逆操作
    cancelRef.current?.focus()
  }, [])

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault()
        onCancel()
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onCancel])

  const cancelLabel = opts.cancelLabel ?? tGlobal('取消')

  return (
    <div
      className="fixed inset-0 z-[60] flex items-center justify-center bg-black/60 p-4"
      role="presentation"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onCancel()
      }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-label={opts.title}
        data-testid="confirm-dialog"
        className="w-full max-w-md rounded-lg border border-line bg-panel shadow-xl"
      >
        <div className="border-b border-line px-4 py-3">
          <div className="flex items-start justify-between gap-2">
            <h2 className="text-sm font-semibold" data-testid="confirm-title">
              {opts.title}
            </h2>
            {/* 出口 ②：右上角 ✕（出口 ① = Esc，出口 ③ = 取消按钮）—— 弹窗三条出口齐备 */}
            <button
              type="button"
              aria-label={tGlobal('关闭')}
              data-testid="confirm-close"
              className="-mr-1 shrink-0 rounded p-0.5 text-ink-dim hover:bg-panel-2 hover:text-ink"
              onClick={onCancel}
            >
              ✕
            </button>
          </div>
          {opts.body && <p className="mt-1 text-xs leading-5 text-ink-dim">{opts.body}</p>}
        </div>

        {(opts.impacts?.length || opts.recoverable) && (
          <div className="space-y-1.5 px-4 py-3 text-xs">
            {opts.impacts?.map((line) => (
              <p key={line} data-testid="confirm-impact" className="flex gap-1.5 leading-5">
                <span className="shrink-0 text-ink-dim">·</span>
                <span>{line}</span>
              </p>
            ))}
            {opts.recoverable && (
              <p
                data-testid="confirm-recoverable"
                className={`mt-1 flex gap-1.5 rounded border border-line bg-panel-2 px-2 py-1.5 leading-5 ${
                  opts.danger ? 'text-amber-400' : 'text-ink-dim'
                }`}
              >
                <span className="shrink-0">↺</span>
                <span>{opts.recoverable}</span>
              </p>
            )}
          </div>
        )}

        <div className="flex items-center justify-end gap-2 border-t border-line px-4 py-3">
          <button
            type="button"
            ref={cancelRef}
            data-testid="confirm-cancel"
            className="rounded border border-line px-3 py-1.5 text-xs text-ink-dim hover:border-accent hover:text-accent"
            onClick={onCancel}
          >
            {cancelLabel}
          </button>
          <button
            type="button"
            data-testid="confirm-ok"
            className={`rounded px-3 py-1.5 text-xs font-medium text-white ${
              opts.danger ? 'bg-red-600 hover:brightness-110' : 'bg-accent hover:brightness-110'
            }`}
            onClick={onConfirm}
          >
            {opts.confirmLabel}
          </button>
        </div>
      </div>
    </div>
  )
}

export function ConfirmProvider({ children }: { children: React.ReactNode }) {
  const [pending, setPending] = useState<Pending | null>(null)
  const awaiting = useRef<Pending | null>(null)

  const confirm = useCallback((opts: ConfirmOptions) => {
    // 已有未决确认：先按「取消」收掉，避免两个对话框叠加
    awaiting.current?.resolve(false)
    return new Promise<boolean>((resolve) => {
      const p: Pending = { opts, resolve }
      awaiting.current = p
      setPending(p)
    })
  }, [])

  const settle = useCallback((ok: boolean) => {
    const p = awaiting.current
    awaiting.current = null
    setPending(null)
    p?.resolve(ok)
  }, [])

  return (
    <ConfirmCtx.Provider value={{ confirm }}>
      {children}
      {pending && (
        <ConfirmDialog opts={pending.opts} onCancel={() => settle(false)} onConfirm={() => settle(true)} />
      )}
    </ConfirmCtx.Provider>
  )
}

export function useConfirm(): ConfirmApi['confirm'] {
  const ctx = useContext(ConfirmCtx)
  if (!ctx) throw new Error('useConfirm must be used within ConfirmProvider')
  return ctx.confirm
}
