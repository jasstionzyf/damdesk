/**
 * 批量/选择动作条（ui-landing §共用组件原则 · KI-16 #5）：图库与搜索结果页共用的**唯一一份**实现，
 * 禁止各页自拼按钮组（文案与顺序不一致正是 KI-16 #5 的根因）。职责只有三件事：
 *
 *  1. 说清范围：已加载 N 张 / 已选 M 张。命中总数可知且大于已加载时，加提示行
 *     「仅作用于已加载的 N 张（共 M 张）」—— 不许笼统地说「范围」「全部」；
 *  2. 全选已加载 / 清空选择（文案钉死；全选的全是**已加载**的卡片，不是命中全集）；
 *  3. 右侧挂页面专属动作：`actions` 只在有选择集时渲染（批量动作作用于选择集），
 *     `trailing` 常驻（如「在图库中打开」）。差异只用 props 表达，不允许 fork 本组件。
 */
import type { ReactNode } from 'react'
import { useI18n } from '../i18n'
import { Button } from './Button'

export interface SelectionBarProps {
  /** 已加载的卡片数（= 全选按钮的选择范围；与网格当前卡片数一致） */
  loaded: number
  /** 当前选择集张数 */
  selected: number
  /**
   * 命中总数。只有**可知**时传（如纯过滤浏览 /api/library/assets 回带的 total）；
   * 不传或 ≤ loaded → 不显示提示行。文搜图/找相似的 k 截断总数不可知，不传。
   */
  total?: number
  onSelectAll: () => void
  onClear: () => void
  /** 页面专属批量动作（有选择集才渲染） */
  actions?: ReactNode
  /** 常驻右侧内容（提示文案 / 次要入口） */
  trailing?: ReactNode
}

export function SelectionBar({
  loaded,
  selected,
  total,
  onSelectAll,
  onClear,
  actions,
  trailing,
}: SelectionBarProps) {
  const { t } = useI18n()
  const capped = total !== undefined && total > loaded
  return (
    <div
      className={`flex shrink-0 flex-wrap items-center gap-2 border-b border-line px-4 py-2 text-xs ${
        selected > 0 ? 'bg-accent-soft/30' : 'bg-panel'
      }`}
      data-testid="selection-bar"
    >
      <span className="text-ink" data-testid="selection-loaded">
        {t('已加载 {n} 张', { n: loaded })}
      </span>
      <span className="text-ink-dim/60" data-testid="selection-count">
        {t('已选 {n}', { n: selected })}
      </span>
      <span className="h-4 w-px bg-line" />
      <Button type="button" data-testid="selection-select-all" onClick={onSelectAll} variant="secondary">
        {t('全选已加载 {n} 张', { n: loaded })}
      </Button>
      <Button
        type="button"
        data-testid="selection-clear"
        disabled={selected === 0}
        onClick={onClear}
        variant="secondary"
      >
        {t('清空选择')}
      </Button>
      {capped && (
        <span className="text-amber-400/90" data-testid="selection-hint" title={t('批量动作只作用于已加载的卡片')}>
          {t('仅作用于已加载的 {loaded} 张（共 {total} 张）', { loaded, total })}
        </span>
      )}
      {selected > 0 && actions}
      {trailing}
    </div>
  )
}
