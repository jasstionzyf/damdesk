/**
 * 浮层原语（唯一实现，别在组件里各写一套）
 *
 * 两层能力，本文件同时提供：
 *
 * ① **关闭语义 / 焦点**（UI-DIALOG 卡 t_9642d1d1，`useModalDismiss` + `ModalCloseButton`）
 *    - Esc：只有「栈顶」弹窗响应 —— 嵌套浮层一次 Esc 只关一层；
 *    - ✕：统一右上角关闭按钮（`ModalCloseButton`，title 带 Esc 提示）；
 *    - 遮罩：只有点击落在遮罩自身（`e.target === e.currentTarget`）才关；
 *    - 焦点：打开记触发元素 → 焦点移进面板 → 关闭归还（元素已卸载则不回焦）。
 *
 * ② **载体壳 + URL 单一真源**（UI-CARRIER 卡 t_b8349d2d，`useOverlay` + `<Dialog>` / `<Drawer>`）
 *    - 开关来自 `?dialog=<name>` / `?drawer=<name>`；打开 push（浏览器 back 优先关浮层），
 *      关闭 replace（不制造历史垃圾）并清掉该浮层的上下文参数（如 `ids`）；
 *    - `aria-labelledby` 指向**渲染出来的可见标题**（不允许 aria-label 只活在属性里）；
 *    - 背景 inert（应用壳 `[data-overlay-base]` 子元素，不含浮层自身）+ body 滚动锁；
 *    - 单一滚动容器：内容多到一屏放不下就换 Drawer / 页面，不许在 Dialog 里套二级滚动。
 *
 * 载体选型判据见 soujpg-ui-patterns §9.2.1：Dialog（≤6 字段一屏、无回看记录、真中断型）／
 * Drawer（字段 ≥7、分区 ≥2、条目数不可控，需可滚动）／页面（列表 + 分页 + 可回看）。
 */
import {
  useCallback,
  useEffect,
  useId,
  useRef,
  type MouseEvent as ReactMouseEvent,
  type ReactNode,
  type RefObject,
} from 'react'
import { createPortal } from 'react-dom'
import { useSearchParams } from 'react-router-dom'
import { tGlobal } from '../i18n'

interface Entry {
  id: number
  close: () => void
}

/** Esc 栈：只让最后挂载（视觉最上层）的那层响应 */
const stack: Entry[] = []
let seq = 0

/** 测试/排查用：当前登记在 Esc 栈上的弹窗数（验证「一次 Esc 只关一层」） */
export function modalStackDepth(): number {
  return stack.length
}

export interface OverlayProps {
  onClick: (e: ReactMouseEvent) => void
  'data-testid': string
}

export interface ModalDismiss<T extends HTMLElement = HTMLDivElement> {
  /** 直接展开到遮罩 div 上：`<div {...overlayProps}>` */
  overlayProps: OverlayProps
  /** 挂到内容面板上：打开即聚焦面板（键盘/读屏直达弹窗），关闭后焦点回到触发元素 */
  contentRef: RefObject<T>
}

export function useModalDismiss<T extends HTMLElement = HTMLDivElement>({
  open,
  onClose,
  suspended = false,
  closeOnOverlay = true,
  overlayTestId = 'modal-overlay',
}: {
  open: boolean
  onClose: () => void
  /** 上层浮层（快捷键帮助等）打开时挂起本层 Esc */
  suspended?: boolean
  /** 是否允许点遮罩关闭（默认 true；整屏预览类可关掉） */
  closeOnOverlay?: boolean
  overlayTestId?: string
}): ModalDismiss<T> {
  // 回调放 ref：父组件每次渲染新建的 onClose 不会导致 Esc 监听反复重挂
  const closeRef = useRef(onClose)
  closeRef.current = onClose

  const contentRef = useRef<T>(null)

  useEffect(() => {
    if (!open || suspended) return
    const entry: Entry = { id: ++seq, close: () => closeRef.current() }
    stack.push(entry)

    // 焦点管理：记住触发元素 → 打开即把焦点移进弹窗 → 关闭时还给触发元素
    const trigger = document.activeElement instanceof HTMLElement ? document.activeElement : null
    const panel = contentRef.current
    if (panel) {
      if (!panel.hasAttribute('tabindex')) panel.setAttribute('tabindex', '-1')
      panel.style.outline = 'none'
      // 组件自己 autoFocus 过就不抢（保留 Enter 直达主按钮的既有手感）
      if (!panel.contains(document.activeElement)) panel.focus({ preventScroll: true })
    }

    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return
      // 不是栈顶 → 让位给上层浮层（不 preventDefault，避免吃掉上层的键位）
      if (stack[stack.length - 1] !== entry) return
      e.preventDefault()
      e.stopPropagation()
      entry.close()
    }
    // capture：早于页面级（冒泡）键位处理，Esc 先到浮层
    window.addEventListener('keydown', onKey, true)
    return () => {
      window.removeEventListener('keydown', onKey, true)
      const i = stack.indexOf(entry)
      if (i >= 0) stack.splice(i, 1)
      // 触发元素还在文档里才还焦点（列表重渲染/删除后不回焦到野节点）
      if (trigger && trigger.isConnected) trigger.focus({ preventScroll: true })
    }
  }, [open, suspended])

  const onClick = useCallback(
    (e: ReactMouseEvent) => {
      if (!closeOnOverlay) return
      // 只认遮罩本身：内容区冒泡上来的点击（含下拉/输入/按钮）不关
      if (e.target !== e.currentTarget) return
      e.stopPropagation()
      closeRef.current()
    },
    [closeOnOverlay],
  )

  return { overlayProps: { onClick, 'data-testid': overlayTestId }, contentRef }
}

/**
 * 统一关闭按钮（右上角 ✕）。
 * `label` 进 aria-label（读屏/自动化选择器都用它），title 附 Esc 提示。
 */
export function ModalCloseButton({
  onClose,
  label = tGlobal('关闭'),
  className = '',
  testId = 'modal-close',
}: {
  onClose: () => void
  label?: string
  className?: string
  /** 覆盖 data-testid（同一页面可能有多种浮层，便于断言具体那一个） */
  testId?: string
}) {
  return (
    <button
      type="button"
      aria-label={label}
      title={`${label}${tGlobal('（Esc）')}`}
      data-testid={testId}
      onClick={onClose}
      className={`shrink-0 rounded p-1 text-ink-dim hover:bg-panel-2 hover:text-ink ${className}`}
    >
      ✕
    </button>
  )
}

// ---------------------------------------------------------------------------------------------
// ② 载体壳 + URL 单一真源（UI-CARRIER / 卡 t_b8349d2d）
// ---------------------------------------------------------------------------------------------

export type OverlayKind = 'dialog' | 'drawer'

/** 应用壳标记：浮层**不能**渲染在这个元素内部（否则 inert 会把自己也锁死） */
export const OVERLAY_BASE_ATTR = 'data-overlay-base'
const BASE_SELECTOR = `[${OVERLAY_BASE_ATTR}]`

function setParamText(params: URLSearchParams, key: string, value: string | number | null | undefined) {
  if (value === null || value === undefined || value === '') {
    params.delete(key)
    return
  }
  params.set(key, String(value))
}

/**
 * 浮层开关（URL 单一真源）。
 *
 *   const ov = useOverlay('drawer', 'packs')
 *   ov.open            // 当前 URL 是否 `?drawer=packs`
 *   ov.openOverlay()   // push：back 关掉它
 *   ov.closeOverlay()  // replace：不制造历史垃圾
 *
 * `extraKeys` 是这个浮层自带的上下文参数（如导出选择集 `ids`），关闭时一并清掉，
 * 避免「关掉浮层但 URL 里留着 ids」导致下次打开带着上一次的选择集。
 */
export function useOverlay(kind: OverlayKind, name: string, extraKeys: string[] = []) {
  const [params, setParams] = useSearchParams()
  const open = params.get(kind) === name
  const extraKeyList = extraKeys.join(',')
  const extraKeyNames = extraKeyList ? extraKeyList.split(',') : []

  const openOverlay = useCallback(
    (extra?: Record<string, string | number | null | undefined>) => {
      const next = new URLSearchParams(params)
      next.set(kind, name)
      for (const [k, v] of Object.entries(extra ?? {})) setParamText(next, k, v)
      setParams(next, { replace: false })
    },
    [params, setParams, kind, name],
  )

  const closeOverlay = useCallback(() => {
    const next = new URLSearchParams(params)
    if (next.get(kind) === name) next.delete(kind)
    for (const k of extraKeyNames) next.delete(k)
    setParams(next, { replace: true })
    // eslint-disable-next-line react-hooks/exhaustive-deps -- extraKeyNames 由 extraKeyList 派生，稳定
  }, [params, setParams, kind, name, extraKeyList])

  return { open, openOverlay, closeOverlay }
}

/** 背景 inert + 滚动锁（只作用于应用壳，不含浮层自身与同级 confirm 对话框） */
function useBackgroundInert(open: boolean) {
  useEffect(() => {
    if (!open || typeof document === 'undefined') return
    const base = document.querySelector(BASE_SELECTOR) as HTMLElement | null
    const prevOverflow = document.body.style.overflow
    base?.setAttribute('inert', '')
    document.body.style.overflow = 'hidden'
    return () => {
      base?.removeAttribute('inert')
      document.body.style.overflow = prevOverflow
    }
  }, [open])
}

export interface OverlayShellProps {
  open: boolean
  /** 可见标题：既渲染成 <h2 id>，也是 aria-labelledby 的目标 */
  title: string
  onClose: () => void
  children: ReactNode
  /** 底部动作区（固定在面板底部，不参与滚动） */
  footer?: ReactNode
  /** 标题下的一句话说明 */
  subtitle?: string
  testId?: string
  /** 上层只读浮层打开时挂起 Esc */
  suspended?: boolean
  /** 点遮罩关闭（默认 true） */
  closeOnMask?: boolean
  /** 面板宽度类 */
  widthClass?: string
}

function OverlayShell({
  kind,
  open,
  title,
  onClose,
  children,
  footer,
  subtitle,
  testId,
  suspended = false,
  closeOnMask = true,
  widthClass,
}: OverlayShellProps & { kind: OverlayKind }) {
  const titleId = useId()
  const shellTestId = testId ?? kind
  // 顺序要紧：inert 的清理必须**先于**焦点归还（面板关闭时触发元素还在 inert 里，
  // 浏览器会拒绝把焦点给 inert 子树中的元素 —— 先摘 inert 再回焦）。
  useBackgroundInert(open)
  const { overlayProps, contentRef } = useModalDismiss<HTMLDivElement>({
    open,
    onClose,
    suspended,
    closeOnOverlay: closeOnMask,
    overlayTestId: `${shellTestId}-mask`,
  })
  if (!open) return null

  const isDrawer = kind === 'drawer'
  const width = widthClass ?? (isDrawer ? 'w-[min(480px,92vw)]' : 'max-w-lg')

  // **必须挂到 document.body**：页面级浮层（如插件详情页的规格编辑）是在 `data-overlay-base` 内部
  // 渲染的，而打开浮层会给 base 加 `inert` —— 浮层自己就在被 inert 的子树里，鼠标点不到、
  // 焦点进不去（真机上表现就是「弹窗里的按钮点不动」）。portal 到 body 后浮层永远在 base 之外。
  const shell = (
    <div
      {...overlayProps}
      className={
        isDrawer
          ? 'fixed inset-0 z-[55] flex justify-end bg-black/40'
          : 'fixed inset-0 z-[55] flex items-center justify-center bg-black/60 p-4'
      }
      data-overlay-mask={kind}
    >
      <div
        ref={contentRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        data-testid={shellTestId}
        data-overlay-kind={kind}
        className={
          isDrawer
            ? `flex h-full ${width} flex-col border-l border-line bg-panel shadow-2xl`
            : `flex max-h-[calc(100vh-6rem)] w-full ${width} flex-col rounded-lg border border-line bg-panel shadow-2xl`
        }
      >
        <header className="flex shrink-0 items-start justify-between gap-2 border-b border-line px-4 py-3">
          <div className="min-w-0">
            <h2 id={titleId} className="truncate text-sm font-semibold" data-testid={`${shellTestId}-title`}>
              {title}
            </h2>
            {subtitle && <p className="mt-1 text-xs leading-5 text-ink-dim">{subtitle}</p>}
          </div>
          <ModalCloseButton onClose={onClose} testId={`${shellTestId}-close`} />
        </header>
        {/* 唯一滚动容器（不许在内容里再套 overflow-y-auto 造二级滚动） */}
        <div className="min-h-0 flex-1 overflow-y-auto overscroll-contain px-4 py-3" data-testid={`${shellTestId}-body`}>
          {children}
        </div>
        {footer && (
          <footer className="flex shrink-0 items-center justify-end gap-2 border-t border-line px-4 py-3">{footer}</footer>
        )}
      </div>
    </div>
  )
  return typeof document === 'undefined' ? shell : createPortal(shell, document.body)
}

// ---------------------------------------------------------------------------------------------
// ③ 非模态停靠抽屉（DOCKED 卡）：检查器专用
// ---------------------------------------------------------------------------------------------

export interface DockedPanelProps {
  open: boolean
  onClose: () => void
  /** 可见标题：既渲染成 <h2 id>，也是 aria-labelledby 的目标 */
  title: string
  subtitle?: string
  testId?: string
  footer?: ReactNode
  children: ReactNode
  /** 上层浮层（Lightbox / Dialog / 快捷键帮助）打开时挂起 Esc，避免一次 Esc 关两层 */
  suspended?: boolean
  /** 宽度类（默认 360，<640 全宽） */
  widthClass?: string
}

/**
 * 右侧**非模态**抽屉：不遮罩、不锁背景、不 trap focus、点面板外不关闭 ——
 * 「边看图边看详情」的那类常驻面板用它，不要用 `<Drawer>`（模态，会盖章遮罩 + inert 背景）。
 *
 * Esc 只挂**冒泡相位**：Lightbox / Dialog 的键位都在 capture 相位，一次 Esc 先归它们，
 * 它们处理完（stopPropagation）本层就收不到 —— 天然满足「Lightbox > dialog > docked > 页面」。
 */
export function DockedPanel({
  open,
  onClose,
  title,
  subtitle,
  testId = 'dock',
  footer,
  children,
  suspended = false,
  widthClass,
}: DockedPanelProps) {
  const titleId = useId()
  // 回调放 ref：父组件每次渲染新建的 onClose 不会导致 Esc 监听反复重挂
  const closeRef = useRef(onClose)
  closeRef.current = onClose

  useEffect(() => {
    if (!open || suspended) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return
      // 输入类控件里的 Esc 先归控件自己（取消输入 / 清空关键词），不吞掉抽屉
      const t = e.target as HTMLElement | null
      if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.tagName === 'SELECT' || t.isContentEditable)) return
      e.preventDefault()
      e.stopPropagation()
      closeRef.current()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [open, suspended])

  if (!open) return null

  return (
    <div
      role="complementary"
      aria-labelledby={titleId}
      data-testid={testId}
      data-overlay-kind="dock"
      className={`fixed inset-y-0 right-0 z-40 flex w-full flex-col border-l border-line bg-panel shadow-2xl ${
        widthClass ?? 'sm:w-[360px]'
      }`}
    >
      <header className="flex shrink-0 items-start justify-between gap-2 border-b border-line px-4 py-3">
        <div className="min-w-0">
          <h2 id={titleId} className="truncate text-sm font-semibold" data-testid={`${testId}-title`}>
            {title}
          </h2>
          {subtitle && (
            <p className="mt-1 text-xs leading-5 text-ink-dim" data-testid={`${testId}-subtitle`}>
              {subtitle}
            </p>
          )}
        </div>
        <ModalCloseButton onClose={onClose} testId={`${testId}-close`} />
      </header>
      {/* 唯一滚动容器 */}
      <div className="min-h-0 flex-1 overflow-y-auto overscroll-contain" data-testid={`${testId}-body`}>
        {children}
      </div>
      {footer && (
        <footer
          data-testid={`${testId}-footer`}
          className="flex shrink-0 items-center justify-end gap-2 border-t border-line px-4 py-3"
        >
          {footer}
        </footer>
      )}
    </div>
  )
}

/** 居中 Dialog（小体量一次性表单 / 只读速查） */
export function Dialog({ testId, ...rest }: Omit<OverlayShellProps, 'testId'> & { testId?: string }) {
  return <OverlayShell {...rest} kind="dialog" testId={testId ?? 'dialog'} />
}

/** 右侧 Drawer（内容中量、需可滚动、需背景可比对） */
export function Drawer({ testId, ...rest }: Omit<OverlayShellProps, 'testId'> & { testId?: string }) {
  return <OverlayShell {...rest} kind="drawer" testId={testId ?? 'drawer'} />
}
