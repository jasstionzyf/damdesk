import type { FilterChip } from './LibraryTree'
import { useI18n } from '../i18n'

/**
 * P1-2：筛选生效时的**醒目横条**（不只是小芯片）。
 *
 * 背景：筛选条件是持久化的（F5/重启后仍生效）。实测踩过——点过「✕ 标签 X 命中 0 张」后回图库
 * imgs=0，用户第一反应是「图丢了/图库空了」，而底部空态文案在长列表里根本看不到。
 * 所以筛选一旦生效，列表**顶部**必须有一行能一眼看懂的横幅：
 *   「筛选中：目录 2024 · 命中 0 张 · 清除」
 *
 * 与芯片行的分工：芯片行负责「逐个条件移除」，横幅负责「当前处于筛选态 + 命中多少 + 一键清除」。
 * 横幅在命中 0 时用警示色（amber），避免和「加载失败」的红色混淆。
 */
export function FilterBanner({
  chips,
  total,
  onRemoveChip,
  onClear,
}: {
  chips: FilterChip[]
  /** 当前筛选条件下的命中总数（服务端 total，不是已加载条数） */
  total: number
  onRemoveChip?: (field: FilterChip['field']) => void
  onClear?: () => void
}) {
  const { t } = useI18n()
  if (chips.length === 0) return null
  const empty = total === 0
  return (
    <div
      data-testid="filter-banner"
      data-empty={empty ? '1' : '0'}
      className={`flex shrink-0 flex-wrap items-center gap-2 border-b px-4 py-1.5 text-xs ${
        empty
          ? 'border-amber-600/50 bg-amber-950/30 text-amber-200'
          : 'border-accent/40 bg-accent-soft/40 text-ink'
      }`}
    >
      <span className="font-medium">
        {t('筛选中：')}{chips.map((c) => c.label).join(' · ')}
      </span>
      <span className={empty ? 'text-amber-300' : 'text-ink-dim'}>
        {empty ? t('命中 0 张 —— 图库不是空的，是当前条件没有匹配') : t('命中 {n} 张', { n: total })}
      </span>
      <span className="ml-auto flex items-center gap-1.5">
        {chips.map((c) => (
          <span
            key={c.field}
            className="inline-flex items-center gap-1 rounded-full border border-line bg-panel-2 py-0.5 pr-1 pl-2 text-ink"
          >
            <span className="max-w-40 truncate">{c.label}</span>
            {onRemoveChip && (
              <button
                type="button"
                aria-label={t('移除筛选：{label}', { label: c.label })}
                data-testid={`chip-remove-${c.field}`}
                onClick={() => onRemoveChip(c.field)}
                className="flex h-4 w-4 shrink-0 items-center justify-center rounded-full text-ink-dim hover:bg-line hover:text-ink"
              >
                ×
              </button>
            )}
          </span>
        ))}
        {onClear && (
          <button
            type="button"
            data-testid="filter-clear"
            onClick={onClear}
            className="rounded border border-line px-2 py-0.5 text-ink-dim hover:border-accent hover:text-accent"
          >
            {t('清除筛选')}
          </button>
        )}
      </span>
    </div>
  )
}
