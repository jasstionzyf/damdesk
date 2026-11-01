import React from 'react'
import { useI18n } from '../i18n'

/** 空状态：图标 + 标题 + 说明 + 引导动作（验收项：空状态均有引导） */
export function EmptyState({
  icon,
  title,
  desc,
  actions,
  hint,
}: {
  icon: string
  title: string
  desc: string
  actions?: React.ReactNode
  hint?: string
}) {
  return (
    <section className="flex flex-1 flex-col items-center justify-center gap-2 px-8 py-20 text-center">
      <div className="flex h-14 w-14 items-center justify-center rounded-full border border-line bg-panel text-2xl text-ink-dim">
        {icon}
      </div>
      <h2 className="mt-2 text-lg font-semibold">{title}</h2>
      <p className="max-w-md text-sm leading-6 text-ink-dim">{desc}</p>
      {actions && <div className="mt-4 flex items-center gap-2">{actions}</div>}
      {hint && (
        <p className="mt-6 rounded-md border border-line bg-panel px-3 py-1.5 text-xs text-ink-dim">{hint}</p>
      )}
    </section>
  )
}

/** 错误态横幅：说明 + 怎么办（验收项：错误态均有引导） */
export function ErrorBanner({
  title,
  detail,
  remedies,
  onRetry,
}: {
  title: string
  detail: string
  remedies: string[]
  onRetry?: () => void
}) {
  const { t } = useI18n()
  return (
    <section className="m-6 rounded-md border border-red-600/50 bg-panel p-4" role="alert">
      <div className="flex items-center gap-2">
        <span className="flex h-5 w-5 items-center justify-center rounded-full bg-red-500 text-[10px] font-bold text-bg">
          ✕
        </span>
        <h3 className="text-sm font-semibold">{title}</h3>
      </div>
      <p className="mt-1.5 pl-7 text-xs leading-5 text-ink-dim">{detail}</p>
      <div className="mt-2 pl-7">
        <p className="text-xs font-medium text-ink">{t('怎么办：')}</p>
        <ul className="mt-1 list-inside list-disc space-y-0.5 text-xs leading-5 text-ink-dim">
          {remedies.map((r) => (
            <li key={r}>{r}</li>
          ))}
        </ul>
      </div>
      {onRetry && (
        <button
          type="button"
          className="mt-3 ml-7 rounded border border-line bg-panel-2 px-3 py-1.5 text-xs hover:border-accent hover:text-accent"
          onClick={onRetry}
        >
          {t('重试连接')}
        </button>
      )}
    </section>
  )
}
