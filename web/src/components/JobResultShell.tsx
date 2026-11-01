/**
 * DM-B2（KI-37）L2 容器：**「这一次的结果」的共用外壳**。
 *
 * 共用：标题/身份副标题、动作区（类型各自的按钮）、收起（回到 L1 列表）、
 * 错误与空态的骨架。分化：只把「结果内容区」交给按 type 注册的视图。
 *
 * 页内展开（不做抽屉/跳页）：开放问题 #2 裁决 = 页内展开起步，零新载体，
 * 关闭即回 L1，选中行与滚动位置都不丢。
 */
import type { ReactNode } from 'react'

import { Button } from './Button'
import { useI18n } from '../i18n'

export interface JobResultShellProps {
  title: string
  subtitle?: string | null
  /** 类型动作（重试/取消/导出/打开文件夹…） */
  actions?: ReactNode
  onClose: () => void
  children: ReactNode
  testid?: string
}

export function JobResultShell({
  title,
  subtitle,
  actions,
  onClose,
  children,
  testid = 'job-result',
}: JobResultShellProps) {
  const { t } = useI18n()
  return (
    <section className="rounded-lg border border-accent/40 bg-panel p-4" data-testid={testid}>
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <h2 className="truncate text-sm font-semibold" data-testid={`${testid}-title`}>
            {title}
          </h2>
          {subtitle && (
            <p className="mt-0.5 truncate text-[11px] text-ink-dim" data-testid={`${testid}-subtitle`}>
              {subtitle}
            </p>
          )}
        </div>
        <div className="flex flex-wrap items-center gap-2">
          {actions}
          <Button
            type="button"
            variant="secondary"
            onClick={onClose}
            data-testid={`${testid}-close`}
          >
            {t('收起')}
          </Button>
        </div>
      </div>
      <div className="mt-3">{children}</div>
    </section>
  )
}
