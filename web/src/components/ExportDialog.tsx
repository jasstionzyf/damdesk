import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { useSearchParams } from 'react-router-dom'
import { api, type ExportReport, type ExportProgress } from '../lib/api'
import { loadExportFilterScope, switchExportDest } from '../lib/export'
import { Dialog, useOverlay } from '../lib/modal'
import { useToast } from '../lib/toast'
import { Button } from './Button'
import { useI18n } from '../i18n'

type Content = 'original' | 'thumb256' | 'thumb1024'

type Scope = 'selection' | 'filter'

const CONTENT_LABEL: Record<Content, string> = {
  original: '原图（按源文件名）',
  thumb256: '缩略图 256',
  thumb1024: '缩略图 1024',
}

function stamp(): string {
  const d = new Date()
  const p = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}-${p(d.getHours())}${p(d.getMinutes())}${p(d.getSeconds())}`
}

/** `?ids=1,2,3` → number[]（脏值丢弃，不炸页面） */
export function parseIdsParam(raw: string | null): number[] {
  if (!raw) return []
  return raw
    .split(',')
    .map((s) => Number(s.trim()))
    .filter((n) => Number.isInteger(n) && n > 0)
}

/**
 * E2 导出对话框（UI-CARRIER-1 改造）：把当前选择集导出为目录 / zip（复用 D11 svc-library::export）。
 *
 * 载体：**Dialog**（判据全中：≤6 字段一次性表单、无二级列表、结果在同屏展示、不需背景对照）。
 * 开关与范围一律走 URL（§9.5 单一真源）：`?dialog=export&ids=1,2,3`
 *  - 深链可直接打开（`/trash?dialog=export&ids=…` 也能用，不绑死图库）；
 *  - 打开 push（back 关掉浮层）、关闭 replace（不制造历史垃圾），关掉时一并清掉 `ids`；
 *  - 外壳统一由 lib/modal.tsx 提供：aria-labelledby 指可见标题、背景 inert + 滚动锁、
 *    Esc/✕/遮罩三通道、关闭归还焦点。本组件不再自己写浮层 chrome。
 * 键盘：Esc 关闭、Tab 走选项、Enter 开始导出（全键盘 = 图库按 E → Enter）。
 */
export function ExportDialog({ suspended = false, onFinished }: {
  onFinished?: () => void
  /** 上层浮层（快捷键帮助）打开时挂起 Esc 处理 */
  suspended?: boolean
}) {
  const { toast } = useToast()
  const ov = useOverlay('dialog', 'export', ['ids'])
  const { t } = useI18n()
  const [params] = useSearchParams()
  const ids = useMemo(() => parseIdsParam(params.get('ids')), [params])
  const open = ov.open

  const [content, setContent] = useState<Content>('original')
  const [mode, setMode] = useState<'dir' | 'zip'>('dir')
  const [dest, setDest] = useState('')
  const [manifest, setManifest] = useState(true)
  /** null = 本次打开还没做过选择 → 按「有选择集默认选择集，否则（有寄存时）默认筛选」推导 */
  const [scopeChoice, setScopeChoice] = useState<Scope | null>(null)
  const [running, setRunning] = useState(false)
  const [progress, setProgress] = useState<ExportProgress | null>(null)
  const [report, setReport] = useState<ExportReport | null>(null)
  const [error, setError] = useState<string | null>(null)
  const pollRef = useRef<number | null>(null)

  // 「当前筛选结果」上下文：打开时读一次（图库 / 搜索 browse 打开入口寄存；深链直开没有 → 选项不出现）
  const filterScope = useMemo(() => (open ? loadExportFilterScope() : null), [open])
  const [filterTotal, setFilterTotal] = useState<number | null>(null)

  // 范围命中数（与导出同源）：M 必须来自**导出真正下发的那份 filter**（DT4），
  // 同一对象加 limit:1 走 /api/library/assets 回读 total —— 不在界面另算一套。
  useEffect(() => {
    if (!open || !filterScope) {
      setFilterTotal(null)
      return
    }
    let stop = false
    setFilterTotal(null)
    api
      .assets({ ...filterScope.filter, limit: 1 })
      .then((r) => {
        if (!stop) setFilterTotal(r.total)
      })
      .catch(() => {
        if (!stop) setFilterTotal(null)
      })
    return () => {
      stop = true
    }
  }, [open, filterScope])

  const scope: Scope =
    scopeChoice ?? (ids.length === 0 && filterScope ? 'filter' : 'selection')

  // 默认目标目录：{dataDir}/exports/export-<时间戳>（后端 dataDir，容器内路径）
  useEffect(() => {
    if (!open) return
    let stop = false
    api
      .appInfo()
      .then((info) => {
        if (stop) return
        setDest((prev) => prev || `${info.dataDir}/exports/export-${stamp()}`)
      })
      .catch(() => {
        if (!stop) setDest((prev) => prev || `/data/exports/export-${stamp()}`)
      })
    return () => {
      stop = true
    }
  }, [open])

  // 打开时同步后台已有任务状态（对话框关闭期间任务可能仍在跑）
  useEffect(() => {
    if (!open) return
    api.exportStatus().then((s) => {
      setRunning(s.running)
      setProgress(s.progress)
      setReport(s.report)
      setError(s.error)
    }).catch(() => {})
  }, [open])

  const stopPoll = useCallback(() => {
    if (pollRef.current !== null) {
      window.clearInterval(pollRef.current)
      pollRef.current = null
    }
  }, [])

  useEffect(() => stopPoll, [stopPoll])

  const startPoll = useCallback(() => {
    stopPoll()
    pollRef.current = window.setInterval(() => {
      void api
        .exportStatus()
        .then((s) => {
          setProgress(s.progress)
          if (!s.running) {
            stopPoll()
            setRunning(false)
            setReport(s.report)
            setError(s.error)
            if (s.report) {
              const r = s.report
              toast(
                r.cancelled ? 'warning' : r.errors.length > 0 ? 'warning' : 'success',
                r.cancelled ? t('导出已取消') : t('导出完成：{a} / {b} 个文件', { a: r.exported, b: r.total }),
                `${r.dest}${r.skipped.length > 0 ? t(' · 跳过 {n}', { n: r.skipped.length }) : ''}${
                  r.excludedRejected > 0 ? t(' · 拒选排除 {n}', { n: r.excludedRejected }) : ''
                }`,
              )
              onFinished?.()
            } else if (s.error) {
              toast('error', t('导出失败'), s.error)
            }
          }
        })
        .catch((e) => {
          stopPoll()
          setRunning(false)
          setError(e instanceof Error ? e.message : String(e))
        })
    }, 400)
  }, [stopPoll, toast, onFinished, t])

  const runStart = useCallback(async () => {
    if (scope === 'selection' && ids.length === 0) {
      toast('warning', t('没有可导出的选择'), t('先在网格里选择图片（单击 / Shift 范围选 / Cmd+A 全选），或切换到「当前筛选结果」'))
      return
    }
    if (scope === 'filter' && !filterScope) {
      // 不可达（选项只在有寄存时渲染）；守住兜底，不允许 filter:undefined 降级成选择集
      toast('warning', t('当前筛选结果不可用'), t('请改用「当前选择集」范围'))
      return
    }
    if (!dest.trim()) {
      toast('warning', t('请填写导出目标路径'), t('目录模式填目录，zip 模式填 .zip 文件路径'))
      return
    }
    setError(null)
    setReport(null)
    setProgress(null)
    try {
      await api.exportStart({
        // 范围二选一（与后端 build_spec 契约一致）：选择集 → assetIds；筛选 → 同一份 filter 对象
        ...(scope === 'filter' ? { filter: filterScope!.filter } : { assetIds: ids }),
        content,
        destDir: mode === 'dir' ? dest.trim() : undefined,
        zipPath: mode === 'zip' ? dest.trim() : undefined,
        manifest,
      })
      setRunning(true)
      startPoll()
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e)
      setError(msg)
      toast('error', t('导出启动失败'), msg)
    }
  }, [scope, ids, filterScope, dest, content, mode, manifest, startPoll, toast, t])

  // 模式切换联动目标路径（task）：目录 ↔ .zip 文件路径同步换形，与导出页同规则
  const changeMode = useCallback((m: 'dir' | 'zip') => {
    setMode(m)
    setDest((d) => switchExportDest(m, d))
  }, [])

  const cancel = useCallback(() => {
    api
      .exportCancel()
      .then(() => toast('info', t('已请求取消导出'), t('在途临时文件会被清理')))
      .catch((e) => toast('error', t('取消失败'), e instanceof Error ? e.message : String(e)))
  }, [toast, t])

  const pct = useMemo(() => {
    if (!progress || progress.total <= 0) return 0
    return Math.min(100, Math.round((progress.done / progress.total) * 100))
  }, [progress])

  const hasStatus = running || !!progress || !!report || !!error

  // 标题随范围走（载体单测钉过「标题必须可见且带真实张数」）
  const title =
    scope === 'filter' && filterScope
      ? t('导出当前筛选结果（{n} 张）', { n: filterTotal === null ? '…' : filterTotal })
      : t('导出选择集（{n} 张）', { n: ids.length })

  // 开始按钮灰置判据：跑动中；选择集范围但没选图；筛选范围但命中数为 0（0 张导出没有意义）
  const startDisabled =
    running || (scope === 'selection' && ids.length === 0) || (scope === 'filter' && filterTotal === 0)

  return (
    <Dialog
      open={open}
      title={title}
      subtitle={t('范围（选择集 / 当前筛选二选一）+ 内容 / 打包 / 目标路径 / 清单文件 manifest.csv。结果进任务中心「导出记录」回看。')}
      onClose={ov.closeOverlay}
      suspended={suspended}
      testId="export-dialog"
      widthClass="max-w-xl"
      footer={
        <>
          <Button
            type="submit"
            form="export-form"
            autoFocus
            disabled={startDisabled}
            variant="primary"
            size="md"
          >
            {running ? t('导出中…') : t('开始导出（Enter）')}
          </Button>
          {running && (
            <Button
              type="button"
              onClick={cancel}
              variant="secondary"
            >
              {t('取消导出')}
            </Button>
          )}
          <Button
            type="button"
            onClick={ov.closeOverlay}
            variant="secondary"
            className="ml-auto"
          >
            {running ? t('后台运行') : t('关闭')}
          </Button>
        </>
      }
    >
      <form
        id="export-form"
        data-testid="export-form"
        onSubmit={(e) => {
          e.preventDefault()
          if (!running) void runStart()
        }}
        className="space-y-3 text-xs"
      >
        {/* 范围（二选一）：只有这两项，不再提供目录/标签/色标/星级等第二套资产筛选 ——
            筛选条件归图库/搜索侧栏，导出只负责「导什么范围、导成什么形态」。 */}
        <div data-testid="export-scope">
          <p className="mb-1.5 text-ink-dim">{t('范围（二选一）')}</p>
          <div className="flex flex-col gap-1.5">
            <label className="flex items-center gap-1.5">
              <input
                type="radio"
                name="export-scope"
                data-testid="export-scope-selection"
                checked={scope === 'selection'}
                onChange={() => setScopeChoice('selection')}
                disabled={running}
              />
              <span data-testid="export-scope-selection-label">
                {t('当前选择集')} <b>{ids.length}</b> {t('张')}
                {ids.length === 0 && <span className="text-ink-dim">{t('（还没选图：单击 / Shift 范围选 / Cmd+A）')}</span>}
              </span>
            </label>
            {filterScope && (
              <label className="flex items-center gap-1.5">
                <input
                  type="radio"
                  name="export-scope"
                  data-testid="export-scope-filter"
                  checked={scope === 'filter'}
                  onChange={() => setScopeChoice('filter')}
                  disabled={running}
                />
                <span data-testid="export-scope-filter-label">
                  {t('当前筛选结果（{label}）', { label: filterScope.label })}{' '}
                  <b data-testid="export-scope-filter-total">{filterTotal === null ? '…' : filterTotal}</b> {t('张')}
                </span>
              </label>
            )}
          </div>
        </div>

        <div>
          <p className="mb-1.5 text-ink-dim">{t('内容')}</p>
          <div className="flex flex-wrap gap-3">
            {(['original', 'thumb256', 'thumb1024'] as Content[]).map((c) => (
              <label key={c} className="flex items-center gap-1.5">
                <input
                  type="radio"
                  name="content"
                  checked={content === c}
                  onChange={() => setContent(c)}
                  disabled={running}
                />
                <span>{t(CONTENT_LABEL[c])}</span>
              </label>
            ))}
          </div>
        </div>

        <div className="flex flex-wrap items-center gap-3">
          <p className="text-ink-dim">{t('打包')}</p>
          <label className="flex items-center gap-1.5">
            <input type="radio" name="mode" checked={mode === 'dir'} onChange={() => changeMode('dir')} disabled={running} />
            <span>{t('目录')}</span>
          </label>
          <label className="flex items-center gap-1.5">
            <input type="radio" name="mode" checked={mode === 'zip'} onChange={() => changeMode('zip')} disabled={running} />
            <span>zip</span>
          </label>
          <label className="ml-auto flex items-center gap-1.5">
            <input
              type="checkbox"
              checked={manifest}
              onChange={(e) => setManifest(e.target.checked)}
              disabled={running}
            />
            <span>{t('写 manifest.csv')}</span>
          </label>
        </div>

        <div>
          <p className="mb-1.5 text-ink-dim">
            {t('目标路径')}{mode === 'dir' ? t('（目录）') : t('（.zip 文件）')}{t(' · 后端机器上的绝对路径')}
          </p>
          <input
            value={dest}
            onChange={(e) => setDest(e.target.value)}
            disabled={running}
            spellCheck={false}
            className="w-full rounded border border-line bg-panel-2 px-2 py-1.5 font-mono text-[11px] outline-none focus:border-accent"
          />
        </div>

        {hasStatus && (
          <div className="rounded border border-line bg-panel-2 p-3" data-testid="export-status">
            {running && (
              <>
                <div className="flex items-center justify-between">
                  <span>
                    {t('导出中 · {phase}', { phase: progress?.phase ?? t('准备') })}
                    {progress && progress.total > 0 ? t(' · {a}/{b}', { a: progress.done, b: progress.total }) : ''}
                  </span>
                  <span className="text-ink-dim">{pct}%</span>
                </div>
                <div className="mt-1.5 h-1.5 w-full overflow-hidden rounded bg-panel">
                  <div className="h-full bg-accent transition-all" style={{ width: `${pct}%` }} />
                </div>
                {progress?.current && (
                  <p className="mt-1.5 truncate font-mono text-[10px] text-ink-dim">{progress.current}</p>
                )}
              </>
            )}
            {!running && report && (
              <>
                <p className="text-sm">
                  {report.cancelled ? t('已取消') : report.errors.length > 0 ? t('完成（有错误）') : t('完成')}{t(' · 导出')}{' '}
                  <span className="font-semibold">{report.exported}</span> / {report.total}
                  {report.skipped.length > 0 ? t(' · 跳过 {n}', { n: report.skipped.length }) : ''}
                  {report.excludedRejected > 0 ? t(' · 拒选排除 {n}', { n: report.excludedRejected }) : ''}
                </p>
                {report.excludedRejected > 0 && (
                  <p className="mt-1 text-[11px] text-ink-dim">
                    {t('拒选资产不交付（留在库里）；清单已记 excludedRejected={n}', { n: report.excludedRejected })}
                  </p>
                )}
                <p className="mt-1 break-all font-mono text-[10px] text-ink-dim">
                  {t('目标：{dest}', { dest: report.dest })}
                  {report.manifestWritten ? t(' · manifest.csv 已写') : ''}
                </p>
                {report.skipped.length > 0 && (
                  <p className="mt-1 text-[11px] text-amber-400">
                    {t('跳过示例：{name}（{reason}）', { name: report.skipped[0].fileName, reason: report.skipped[0].reason })}
                  </p>
                )}
                {report.errors.length > 0 && <p className="mt-1 text-[11px] text-red-400">{t('错误：{err}', { err: report.errors[0] })}</p>}
              </>
            )}
            {!running && error && <p className="text-[11px] text-red-400">{t('错误：{err}', { err: error })}</p>}
          </div>
        )}
      </form>
    </Dialog>
  )
}
