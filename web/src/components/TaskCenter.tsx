import { useEffect, useState } from 'react'
import { useI18n } from '../i18n'
import { TASK_KIND_LABEL, useTasks, type Task } from '../lib/tasks'
import { Button } from './Button'
import { PathLink } from './PathLink'

function ProgressBar({ current, total }: { current: number; total: number }) {
  const pct = total > 0 ? Math.min(100, Math.round((current / total) * 100)) : 0
  return (
    <div className="mt-2 h-1.5 w-full overflow-hidden rounded-full bg-panel">
      <div className="h-full rounded-full bg-accent transition-all" style={{ width: `${pct}%` }} />
    </div>
  )
}

function TaskRow({ t, focusTaskId }: { t: Task; focusTaskId?: number | null }) {
  const { t: tr } = useI18n()
  const { cancel, retry, retryFailed, dismiss } = useTasks()
  const [showFailures, setShowFailures] = useState(false)
  const pct = t.total > 0 ? Math.min(100, Math.round((t.current / t.total) * 100)) : 0
  const failed = t.failed ?? 0
  const items = t.failedItems ?? []
  /** 部分失败：批次跑完了但有张失败——不能按「✓ 完成」呈现 */
  const partial = t.status === 'done' && failed > 0
  const canShowFailures = (partial || t.status === 'error' || t.status === 'cancelled') && items.length > 0

  // toast「查看失败明细」→ 抽屉打开时就地展开这一行
  useEffect(() => {
    if (focusTaskId === t.id) setShowFailures(true)
  }, [focusTaskId, t.id])

  return (
    <div
      className={`rounded-md border p-3 ${
        partial ? 'border-amber-600/50 bg-amber-950/20' : 'border-line bg-panel-2'
      }`}
      data-testid={`task-row-${t.kind}-${t.id}`}
      data-task-failed={failed}
    >
      <div className="flex items-center gap-2">
        <span className="rounded bg-panel px-1.5 py-0.5 text-[10px] text-ink-dim">{tr(TASK_KIND_LABEL[t.kind])}</span>
        <p className="min-w-0 flex-1 truncate text-sm">{t.title}</p>
        {t.status === 'running' && (
          <Button
            type="button"
            onClick={() => cancel(t.id)}
            variant="secondary"
            size="none"
            tone="danger"
            className="shrink-0 px-2 py-0.5 text-xs"
          >
            取消
          </Button>
        )}
        {t.status !== 'running' && (
          <Button
            type="button"
            aria-label={tr(tr('移除记录'))}
            onClick={() => dismiss(t.id)}
            variant="icon"
            size="none"
            className="shrink-0"
          >
            ✕
          </Button>
        )}
      </div>
      {t.detail && <p className="mt-1 truncate text-xs text-ink-dim">{t.detail}</p>}

      {t.status === 'running' && <ProgressBar current={t.current} total={t.total} />}

      <div className="mt-1.5 flex items-center justify-between text-xs">
        {t.status === 'running' && (
          <span className="text-ink-dim tabular-nums">
            {t.current} / {t.total}（{pct}%）
            {failed > 0 && <span className="ml-1 text-amber-400">· 已失败 {failed}</span>}
          </span>
        )}
        {partial && (
          <span className="text-amber-300">
            ⚠ 完成 · {failed} 张失败 · 共 {t.total} 项
          </span>
        )}
        {t.status === 'done' && failed === 0 && <span className="text-emerald-400">✓ 完成 · 共 {t.total} 项</span>}
        {t.status === 'cancelled' && <span className="text-ink-dim">已取消 · 已处理 {t.current} 项</span>}
        {t.status === 'error' && <span className="text-red-400">失败于第 {t.current} 项</span>}
        {canShowFailures && (
          <Button
            type="button"
            data-testid={`toggle-failures-${t.id}`}
            onClick={() => setShowFailures((v) => !v)}
            variant="link"
            size="none"
            className="shrink-0 px-1.5 py-0.5 text-xs"
          >
            {showFailures ? '收起失败明细' : `失败明细（${failed || items.length}）`}
          </Button>
        )}
      </div>

      {showFailures && items.length > 0 && (
        <ul className="mt-2 max-h-40 space-y-1 overflow-y-auto rounded border border-line bg-panel p-2" data-testid={`failure-list-${t.id}`}>
          {items.map((f) => (
            <li key={f.path} className="text-xs leading-4" data-testid="failure-item">
              <span className="text-ink">{f.fileName}</span>
              <span className="ml-1 text-red-300/90" title={f.error}>
                {f.error}
              </span>
              {/* KI-10：失败项的文件路径 = reveal（修好权限/占用前先定位到底是哪个文件） */}
              <PathLink path={f.path} mode="reveal" className="mt-0.5" />
            </li>
          ))}
          {failed > items.length && (
            <li className="text-xs text-ink-dim">仅列出前 {items.length} 张（本批共 {failed} 张失败）</li>
          )}
        </ul>
      )}

      {(t.status === 'error' || partial) && (
        <div className="mt-2 rounded border border-red-600/40 bg-red-950/30 p-2">
          {t.status === 'error' && <p className="text-xs leading-5 text-red-300">{t.error}</p>}
          <div className="mt-1.5 flex flex-wrap items-center gap-3">
            {t.kind === 'import' && items.length > 0 && (
              <Button
                type="button"
                data-testid={`retry-failed-${t.id}`}
                onClick={() => retryFailed(t.id)}
                variant="secondary"
                size="none"
                className="px-2 py-0.5 text-xs"
              >
                重试失败项（{items.length}）
              </Button>
            )}
            <Button
              type="button"
              onClick={() => retry(t.id)}
              variant="secondary"
              size="none"
              className="px-2 py-0.5 text-xs"
            >
              {t.kind === 'import' ? '重跑整批' : '重试'}
            </Button>
            <span className="text-xs text-ink-dim">
              {t.kind === 'import'
                ? tr(tr('修好文件权限/占用后可直接重试失败项，不用重扫整个目录'))
                : tr(tr('连续失败可在设置页检查 tag provider 配置'))}
            </span>
          </div>
        </div>
      )}
    </div>
  )
}

/**
 * DM-B2（KI-37）L2：单条后台任务的明细（失败清单 + 重试 / 取消）。
 * 任务中心把「一次执行」当一行，点开就是这一行自己的明细 —— 不再是一页里堆两套列表。
 */
export function TaskDetail({ taskId }: { taskId: number }) {
  const { t: tr } = useI18n()
  const { tasks } = useTasks()
  const t = tasks.find((x) => x.id === taskId) ?? null
  if (!t) {
    return (
      <p className="text-xs text-ink-dim" data-testid="task-detail-empty">
        {tr(tr('这条记录不在了（关掉页面就会清掉本地记录）。'))}
      </p>
    )
  }
  return (
    <div data-testid="task-detail">
      <TaskRow t={t} focusTaskId={taskId} />
    </div>
  )
}

/**
 * 任务列表（UI-CARRIER-1）：任务中心从 Drawer 改成页面 `/tasks` 后的可复用主体。
 *
 * 为什么不留在 Drawer（见 reports/UI-CARRIER.md 载体自检表）：任务是「列表 + 可回看 + 需要
 * 逐行展开失败明细」的结构，违反 Dialog 判据 #1/#4；且抽屉宽 384px 时失败明细行挤成两行，
 * 用户没法照着改文件名。页面载体同时给了深链（`/tasks?task=<id>`）与刷新保留。
 */
export function TaskList({
  focusTaskId,
  emptyTitle,
  emptyDesc,
}: {
  /** toast「查看失败明细」定位到的那一行（进页面即展开失败明细） */
  focusTaskId?: number | null
  emptyTitle?: string
  emptyDesc?: string
}) {
  const { t: tr } = useI18n()
  const { tasks } = useTasks()
  const effEmptyTitle = emptyTitle ?? tr(tr('暂无后台工作'))
  const effEmptyDesc =
    emptyDesc ?? tr(tr('导入、打标、去重、导出的进度会集中展示在这里，可随时取消；失败的项可一键重试'))
  const order = { running: 0, error: 1, cancelled: 2, done: 3 } as const
  const sorted = [...tasks].sort((a, b) => order[a.status] - order[b.status] || b.startedAt - a.startedAt)

  if (sorted.length === 0) {
    return (
      <div className="flex flex-col items-center gap-2 py-16 text-center" data-testid="task-list-empty">
        <span className="text-2xl text-ink-dim">ⓘ</span>
        <p className="text-sm text-ink-dim">{effEmptyTitle}</p>
        <p className="max-w-[320px] text-xs leading-5 text-ink-dim/80">{effEmptyDesc}</p>
      </div>
    )
  }
  return (
    <div className="space-y-2" data-testid="task-list">
      {sorted.map((t) => (
        <TaskRow key={t.id} t={t} focusTaskId={focusTaskId} />
      ))}
    </div>
  )
}

/** 顶栏触发按钮 + 自定义事件监听（toast「查看任务中心」联动） */
export function TaskCenterButton({ onClick }: { onClick: (taskId?: number) => void }) {
  const { t: tr } = useI18n()
  const { runningCount } = useTasks()
  const [flash, setFlash] = useState(false)
  const { tasks } = useTasks()
  const hasError = tasks.some((t) => t.status === 'error')
  /** 部分失败（跑完但有张失败）也要在顶栏亮起来，否则用户以为一切正常 */
  const hasPartial = tasks.some((t) => t.status === 'done' && (t.failed ?? 0) > 0)
  const hasWarn = hasError || hasPartial

  useEffect(() => {
    const h = (e: Event) => {
      const detail = (e as CustomEvent<{ taskId?: number } | undefined>).detail
      onClick(detail?.taskId)
    }
    window.addEventListener('dam:open-task-center', h)
    return () => window.removeEventListener('dam:open-task-center', h)
  }, [onClick])

  useEffect(() => {
    if (runningCount > 0) {
      setFlash(true)
      const t = window.setTimeout(() => setFlash(false), 500)
      return () => window.clearTimeout(t)
    }
  }, [runningCount])

  return (
    // 顶栏任务入口：统一 Button 载体（KI-8b）。状态语义走 tone（失败=红）+ 角标点，不再自绘 border-line/ink-dim
    <Button
      type="button"
      onClick={() => onClick()}
      variant="secondary"
      tone={hasError ? 'danger' : 'default'}
      className={`relative gap-1.5 px-2.5 py-1.5 text-sm ${flash ? 'ring-1 ring-accent' : ''}`}
      title={hasError ? tr(tr('有任务失败（点开看明细）')) : hasPartial ? tr('有任务部分失败（点开看明细）') : tr('处理记录')}
    >
      <span>{tr(tr('☰ 处理记录'))}</span>
      {runningCount > 0 && (
        <span className="absolute -top-1.5 -right-1.5 flex h-4 min-w-4 items-center justify-center rounded-full bg-accent px-1 text-[10px] font-bold text-white">
          {runningCount}
        </span>
      )}
      {hasWarn && runningCount === 0 && (
        <span
          className={`absolute -top-1.5 -right-1.5 h-2.5 w-2.5 rounded-full ${hasError ? 'bg-red-500' : 'bg-amber-500'}`}
          data-testid="task-center-warn-dot"
        />
      )}
    </Button>
  )
}
