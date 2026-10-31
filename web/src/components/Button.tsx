/**
 * 统一按钮载体（KI-8 / 卡 task）。
 *
 * 判据来自 skill `UI 规范` §9.8「可点性分级」：
 *   **静止态（不 hover、不 focus）就必须能区分可点与不可点。**
 * 之前全仓 100+ 处手写 `rounded border border-line px-3 py-1.5 text-xs text-ink-dim`，
 * 静止态 = 灰字 + 淡边，与普通文本块几乎同视觉，只有 hover 才变化（触屏/键盘用户看不到）。
 *
 * 四个载体（不允许混用、不允许再加第五种「裸文字动作」）：
 *   primary   —— 每屏/每浮层最多 1 个主推进动作：实心 accent 底 + 白字
 *   secondary —— 绝大多数常规动作：可见描边（ink-dim/60，对底 ≥3:1）+ panel-2 提亮底 + ink 字
 *   icon      —— 工具栏 / 行内图标动作：32×32 热区 + 同二级的描边与底；纯图标必须带 title/aria-label
 *   link      —— 仅限表格内跳转 / 面包屑：accent 色 + 下划线（动作不得做成裸文字）
 *
 * 四态：default / hover / active（选中，用 accent-soft 底色承载）/ disabled（opacity-50 保留形状，
 * 不许「变文字」）。键盘 focus = 2px accent ring + offset 2，全仓唯一实现点。
 */
import type { ButtonHTMLAttributes } from 'react'

export type ButtonVariant = 'primary' | 'secondary' | 'icon' | 'link'
export type ButtonSize = 'sm' | 'md' | 'none'
export type ButtonTone = 'default' | 'danger'

export interface ButtonClassOptions {
  variant?: ButtonVariant
  size?: ButtonSize
  tone?: ButtonTone
  /** 选中/开关态（aria-pressed=true）：用底色区分，不只换字色 */
  active?: boolean
  /** 追加布局类（宽度、外边距、圆角等），只许加不改载体语义 */
  className?: string
}

/** 载体公共部分：布局 + 交互态 + focus ring（唯一实现点，页面不要重复写） */
const BASE =
  'inline-flex cursor-pointer select-none items-center justify-center gap-1.5 rounded font-medium ' +
  'transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent ' +
  'focus-visible:ring-offset-2 focus-visible:ring-offset-bg ' +
  'disabled:cursor-not-allowed disabled:opacity-50'

/** 尺寸只决定几何；icon 变体固定 32×32 热区（§9.8 触屏最小热区） */
function geometry(variant: ButtonVariant, size: ButtonSize): string {
  if (size === 'none') return ''
  if (variant === 'icon') return 'h-8 w-8 p-0 text-xs'
  if (variant === 'link') return 'h-8 px-1 text-xs'
  return size === 'md' ? 'h-9 px-3 text-sm' : 'h-8 px-2.5 text-xs'
}

/**
 * 静态可辨信号（无 hover 也成立）：
 * - primary：实心 accent 底（对暗底明显）；
 * - secondary/icon：ink-dim/60 描边 —— 合成色 #68707c 对 bg 3.85:1、对 panel 3.46:1，
 *   满足 §9.8「去色后按钮边界与背景对比 ≥3:1」；再叠 panel-2 提亮底；
 * - 字色一律 ink（不许用 ink-dim 当按钮字色）。
 */
function skin(variant: ButtonVariant, tone: ButtonTone, active: boolean): string {
  if (variant === 'primary') {
    return active
      ? 'bg-accent/85 text-white hover:bg-accent/90'
      : 'bg-accent text-white hover:bg-accent/90 active:bg-accent/80'
  }
  if (variant === 'link') {
    return 'text-accent underline underline-offset-2 hover:text-accent/80 active:text-accent'
  }
  const danger = tone === 'danger'
  if (active) {
    return danger
      ? 'border border-red-400 bg-red-500/15 text-red-200'
      // 选中态用 accent-soft 底色承载（不只换字色）；字色保持 ink，accent 字压在 accent-soft 上只有 2.73:1
      : 'border border-accent bg-accent-soft text-ink'
  }
  if (danger) {
    return 'border border-red-400/70 bg-panel-2 text-red-300 hover:border-red-400 hover:bg-red-500/10 hover:text-red-200'
  }
  return 'border border-ink-dim/60 bg-panel-2 text-ink hover:border-accent hover:text-accent active:bg-accent-soft/40'
}

/** 非 <button> 载体（<Link>/<a>）复用同一套类：避免 button 套 a 的非法结构 */
export function buttonClass(opts: ButtonClassOptions = {}): string {
  const { variant = 'secondary', size = 'sm', tone = 'default', active = false, className } = opts
  return [BASE, geometry(variant, size), skin(variant, tone, active), className].filter(Boolean).join(' ')
}

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement>, ButtonClassOptions {}

export function Button({
  variant = 'secondary',
  size = 'sm',
  tone = 'default',
  active,
  className,
  type = 'button',
  children,
  ...rest
}: ButtonProps) {
  return (
    <button
      type={type}
      // 只在调用方显式传 active 时才声明开关语义（普通动作按钮不该有 aria-pressed）
      aria-pressed={active === undefined ? undefined : active}
      className={buttonClass({ variant, size, tone, active: active === true, className })}
      {...rest}
    >
      {children}
    </button>
  )
}
