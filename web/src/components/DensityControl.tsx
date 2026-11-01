import { useI18n } from '../i18n'
/**
 * 网格密度控制（UI-LAYOUT / 卡 t_11662290；规范 soujpg-ui-patterns §8.4 / §9.6）
 *
 * 设计口径（与规范钉死一致，页面里别再各自发明）：
 *  1. 密度 = **缩略图最小列宽**（px），不是「列数」。列数永远由容器宽度算（CSS `auto-fill`），
 *     所以窗口缩放/侧栏折叠时列数自己变，不会出现「写死 4 列 → 窄屏溢出」。
 *  2. 三档 S/M/L 是标称档（Eagle / Lightroom 同款），URL 里存的是实际 px（`thumb`），
 *     这样拖滑块（将来）和点档位共用同一真源。
 *  3. 值进 URL 由 `useUrlState` 保证单一真源（§9.5）；localStorage 只作为「裸链接」的默认偏好。
 *  4. 列数计算与 CSS `repeat(auto-fill, minmax(min, 1fr))` + `gap` 的语义**逐字对齐**
 *     （`columnsFor` 就是它的整数版），否则「算出来 4 列、渲染成 3 列」。
 */

/** 三档标称值（S/M/L） */
export const DENSITY_PRESETS = [
  { key: 'S', label: '小', thumb: 130 },
  { key: 'M', label: '中', thumb: 170 },
  { key: 'L', label: '大', thumb: 220 },
] as const

export type DensityKey = (typeof DENSITY_PRESETS)[number]['key']

/** 合法区间：低于 110 缩略图无信息量，高于 260 一屏只剩一列 */
export const THUMB_MIN = 110
export const THUMB_MAX = 260
/** 默认档 M —— 与 §9.6「≥1280 主内容 ≥4 列、列宽 ≥170px」直接对应 */
export const DEFAULT_THUMB = 170
/** 密度偏好的 localStorage 键（唯一真源：图库读它兜底裸链接，设置页写它） */
export const DENSITY_PREF_KEY = 'dam.gallery.thumb'
/** 与网格 `gap-3`（Tailwind 0.75rem = 12px）保持一致 */
export const GRID_GAP = 12

/** 把任意来源（URL / localStorage / 滑块）的数值夹到合法区间；空值/非法值回默认档 */
export function clampThumb(n: unknown): number {
  // null / undefined / '' 是「没有值」而不是 0 —— 别把缺省值夹成最小档
  if (n === null || n === undefined || n === '') return DEFAULT_THUMB
  const v = typeof n === 'number' ? n : Number(n)
  if (!Number.isFinite(v)) return DEFAULT_THUMB
  return Math.min(THUMB_MAX, Math.max(THUMB_MIN, Math.round(v)))
}

/**
 * 容器宽度 → 列数：`repeat(auto-fill, minmax(thumb, 1fr))` 的行内副本。
 * width 无效（0 / NaN / 负）→ 0 列（还没有布局，别谎报 1 列）。
 */
export function columnsFor(width: number, thumb: number = DEFAULT_THUMB, gap: number = GRID_GAP): number {
  if (!Number.isFinite(width) || width <= 0) return 0
  const t = clampThumb(thumb)
  const g = Number.isFinite(gap) && gap >= 0 ? gap : GRID_GAP
  // n 列的占宽 = n*thumb + (n-1)*gap ≤ width  ⇔  n ≤ (width+gap)/(thumb+gap)
  return Math.max(1, Math.floor((width + g) / (t + g)))
}

/** 当前 px 命中的标称档（用于按钮态；不在标称值上时返回 '' —— 将来滑块不会假装是档位） */
export function presetOf(thumb: number): DensityKey | '' {
  const hit = DENSITY_PRESETS.find((p) => p.thumb === clampThumb(thumb))
  return hit ? hit.key : ''
}

/**
 * 密度控件（工具条右侧）。
 * 纯展示 + 回调：值从 URL 来、改值写 URL，本组件不持有状态（避免两处真源）。
 */
export function DensityControl({
  thumb,
  columns,
  onChange,
}: {
  thumb: number
  /** 当前列数（仅供人读数，缺省不显示） */
  columns?: number
  onChange: (thumb: number) => void
}) {
  const { t } = useI18n()
  const cur = clampThumb(thumb)
  const preset = presetOf(cur)
  return (
    <span
      role="group"
      aria-label={t('网格密度')}
      data-testid="density-control"
      data-thumb={cur}
      className="flex items-center gap-1"
    >
      <span className="text-[11px] text-ink-dim">{t('密度')}</span>
      {DENSITY_PRESETS.map((p) => {
        const active = preset === p.key
        return (
          <button
            key={p.key}
            type="button"
            data-testid={`density-${p.key}`}
            aria-pressed={active}
            aria-label={t('缩略图{size}（列宽 {thumb}px）', { size: t(p.label), thumb: p.thumb })}
            title={t('缩略图{size}：最小列宽 {thumb}px（列数由容器宽度自动算）', { size: t(p.label), thumb: p.thumb })}
            onClick={() => onChange(p.thumb)}
            className={`rounded border px-1.5 py-0.5 text-[11px] ${
              active
                ? 'border-accent bg-accent-soft text-ink'
                : 'border-line text-ink-dim hover:border-accent hover:text-ink'
            }`}
          >
            {p.key}
          </button>
        )
      })}
      <span className="w-14 text-[11px] tabular-nums text-ink-dim/80" data-testid="density-readout">
        {cur}px{columns ? t(' · {n} 列', { n: columns }) : ''}
      </span>
    </span>
  )
}
