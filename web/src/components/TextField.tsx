import { forwardRef, useCallback, useEffect, useImperativeHandle, useRef, useState } from 'react'

/**
 * 基础文本输入（全站唯一实现）—— 页面不要再各自手写 `<input>`。
 *
 * 这里钉死的是**踩过的坑**，不是风格偏好：
 *
 *  1. **受控输入必须在每次 input 事件里同步可见值**。禁止在 composition 期间 `if (composing) return`
 *     掉 onChange：React 下一次渲染会把 DOM 值写回 `value`（旧串）→ 用户看到「输入框打不出字」。
 *  2. **拼音中间态只用事件自带的 `isComposing` 判断**，不要用 `compositionstart → ref = true`
 *     那种闩锁——`compositionend` 一旦没到（切窗口 / 输入法取消 / Windows 输入法事件顺序异常），
 *     闩锁永不释放，此后所有输入都被吞，输入框彻底废掉。
 *  3. **焦点要保持**。父级常用 `key={sig}` 强制重建子树（例如图库 `AssetGrid key={loadSig}`）来重置滚动，
 *     子输入框会跟着重建 → 浏览器把焦点丢到 `body`，用户继续打字没有任何反应。
 *     传 `focusKey` 即自动登记焦点/光标，重建后由新实例取回（`isConnected=false` 的重建型 blur 不算离开）。
 *  4. **提交口径统一**：防抖窗口 / Enter / 失焦 / Esc 四个口子都在组件内，页面只实现 `onCommit`。
 *     显示与提交解耦：`draft` 管显示，`value` 是外部真源（URL / 父 state）。
 *
 * 行为约定：
 *  - `commitDelayMs > 0`：停止输入这么久后提交（配合 `onDraftChange` 判断「查询在途」）；
 *  - `commitDelayMs = 0`（默认）：不自动提交，只在 Enter / 失焦时提交（普通表单输入框用这档）；
 *  - 删到空串永远立即提交（用户明确动作，不排队）；
 *  - Esc：提交空串并失焦（输入框内的「清空」语义）；
 *  - 自己刚提交的那次 `value` 回传**不回灌** draft（否则会冲掉同一拍里又敲的字）。
 */

interface FocusRecord {
  /** 光标位置（重建后还给用户） */
  caret: number
  /** 可见值（重建发生在提交前时，别把用户敲的字弄丢） */
  text: string
  /** 登记时间：过期记录不再抢焦点（避免上次会话残留导致下次进页面莫名其妙聚焦） */
  at: number
}

const focusRegistry = new Map<string, FocusRecord>()
/** 重建 → 新实例挂载通常在几十毫秒内；超过这个窗口就认为是过期记录 */
const FOCUS_RESTORE_TTL_MS = 3000

export interface TextFieldProps
  extends Omit<
    React.InputHTMLAttributes<HTMLInputElement>,
    'value' | 'defaultValue' | 'onChange' | 'onBlur' | 'onFocus' | 'onKeyDown' | 'onCompositionEnd'
  > {
  /** 已提交值（外部真源） */
  value: string
  /** 提交：防抖窗口到点 / Enter / 失焦 / Esc 清空 */
  onCommit?: (v: string) => void
  /** 可见值变化（含拼音中间态）—— 页面用它判断「还没提交」 */
  onDraftChange?: (v: string) => void
  /** 防抖窗口（ms）；0 = 只在 Enter / 失焦时提交 */
  commitDelayMs?: number
  /** 跨父级重建保持焦点的身份键（同一个键的实例视作同一个框） */
  focusKey?: string
  onFocus?: React.FocusEventHandler<HTMLInputElement>
  onBlur?: React.FocusEventHandler<HTMLInputElement>
  onKeyDown?: React.KeyboardEventHandler<HTMLInputElement>
  onCompositionEnd?: React.CompositionEventHandler<HTMLInputElement>
}

export const TextField = forwardRef<HTMLInputElement, TextFieldProps>(function TextField(
  {
    value,
    onCommit,
    onDraftChange,
    commitDelayMs = 0,
    focusKey,
    onFocus,
    onBlur,
    onKeyDown,
    onCompositionEnd,
    ...rest
  },
  ref,
) {
  const innerRef = useRef<HTMLInputElement | null>(null)
  useImperativeHandle(ref, () => innerRef.current as HTMLInputElement, [])

  const [draft, setDraft] = useState(value)
  const draftRef = useRef(value)
  /** 我们最后一次交给外部的值：用于区分「自己提交的回传」和「外部变化（back/forward、深链、芯片清除）」 */
  const lastSent = useRef(value)
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null)

  const applyDraft = useCallback(
    (v: string) => {
      draftRef.current = v
      setDraft(v)
      onDraftChange?.(v)
    },
    [onDraftChange],
  )

  /** 立即提交（顺带把 draft 落定） */
  const flush = useCallback(
    (v: string) => {
      if (timer.current) {
        clearTimeout(timer.current)
        timer.current = null
      }
      applyDraft(v)
      lastSent.current = v
      onCommit?.(v)
    },
    [applyDraft, onCommit],
  )

  /** 排静默窗口；空串立即提交 */
  const schedule = useCallback(
    (v: string) => {
      if (timer.current) {
        clearTimeout(timer.current)
        timer.current = null
      }
      if (commitDelayMs <= 0) return
      if (v.trim() === '') {
        flush(v)
        return
      }
      timer.current = setTimeout(() => {
        timer.current = null
        lastSent.current = v
        applyDraft(v)
        onCommit?.(v)
      }, commitDelayMs)
    },
    [applyDraft, commitDelayMs, flush, onCommit],
  )

  useEffect(
    () => () => {
      if (timer.current) clearTimeout(timer.current)
    },
    [],
  )

  /** 外部值变化 → 回灌 draft；自己刚提交的那次不回灌 */
  useEffect(() => {
    if (value === lastSent.current) return
    lastSent.current = value
    applyDraft(value)
  }, [value, applyDraft])

  /** 重建后首次挂载：先把没提交的可见值认回来（不然父级重建会把用户敲的字弄丢） */
  const restored = useRef(false)
  useEffect(() => {
    if (!focusKey || restored.current) return
    restored.current = true
    const rec = focusRegistry.get(focusKey)
    if (!rec || Date.now() - rec.at > FOCUS_RESTORE_TTL_MS) return
    if (rec.text !== draftRef.current) applyDraft(rec.text)
  })

  /** 父级用 key 重建子树后：把焦点与光标还给用户（否则「接着打字没反应」） */
  useEffect(() => {
    if (!focusKey) return
    const el = innerRef.current
    if (!el || document.activeElement === el) return
    const rec = focusRegistry.get(focusKey)
    if (!rec || Date.now() - rec.at > FOCUS_RESTORE_TTL_MS) return
    el.focus()
    const n = Math.min(rec.caret, el.value.length)
    try {
      el.setSelectionRange(n, n)
    } catch {
      /* 部分浏览器在失焦态调用会抛：忽略，焦点已经在框里了 */
    }
  })

  /** 记住焦点与光标（重建后原样还回去） */
  const rememberFocus = useCallback(() => {
    if (!focusKey) return
    const el = innerRef.current
    if (!el || document.activeElement !== el) return
    focusRegistry.set(focusKey, {
      caret: el.selectionStart ?? el.value.length,
      text: el.value,
      at: Date.now(),
    })
  }, [focusKey])

  const handleFocus = (e: React.FocusEvent<HTMLInputElement>) => {
    rememberFocus()
    onFocus?.(e)
  }

  const handleBlur = (e: React.FocusEvent<HTMLInputElement>) => {
    if (focusKey) {
      const el = e.currentTarget
      focusRegistry.set(focusKey, {
        caret: el.selectionStart ?? el.value.length,
        text: el.value,
        at: Date.now(),
      })
      // blur 的成因要**下一个 tick 再判**：父级 key 重建时 Chromium 会在节点仍连着的时候派发 blur，
      // 同步看 isConnected 会把「重建」误判成「用户离开」→ 焦点就再也还不回来了。
      setTimeout(() => {
        if (el.isConnected) focusRegistry.delete(focusKey)
      }, 0)
    }
    // 真失焦 = 把没提交的字落地（输入法被切走 / 静默窗口没到就切窗口）
    if (e.currentTarget.isConnected && draftRef.current.trim() !== value.trim()) flush(draftRef.current)
    onBlur?.(e)
  }

  const handleChange = (e: React.ChangeEvent<HTMLInputElement>) => {
    const v = e.target.value
    applyDraft(v)
    rememberFocus()
    // 拼音中间态：只显示不提交（不阻断 onChange，见文件头第 1 条）
    if ((e.nativeEvent as InputEvent).isComposing) return
    schedule(v)
  }

  const handleCompositionEnd = (e: React.CompositionEvent<HTMLInputElement>) => {
    const v = e.currentTarget.value
    applyDraft(v)
    rememberFocus()
    schedule(v)
    onCompositionEnd?.(e)
  }

  const handleKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Enter' && !(e.nativeEvent as KeyboardEvent).isComposing) {
      e.preventDefault()
      flush(draftRef.current)
    } else if (e.key === 'Escape') {
      flush('')
      e.currentTarget.blur()
    }
    onKeyDown?.(e)
  }

  return (
    <input
      {...rest}
      ref={innerRef}
      value={draft}
      onChange={handleChange}
      onCompositionEnd={handleCompositionEnd}
      onFocus={handleFocus}
      onBlur={handleBlur}
      onKeyDown={handleKeyDown}
    />
  )
})
