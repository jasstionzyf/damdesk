/**
 * STORAGE-MIGRATE-1 前端入口（DT7 / 卡 task）：**一键迁入库**。
 *
 * 解决的是「当初选了原地索引（reference），现在想让 DAM 自己保管一份」的诉求：
 * 把原图复制进库根（`libraryRoot`，与 copyin 导入同布局），并把资产切到 `storeMode=copyin`。
 * 后端能力见 DT6（`POST /api/library/migrate-to-library` + `.../status`）。
 *
 * 载体：**Dialog**（≤6 字段一次性动作、结果同屏、无二级列表需要回看 → 判据见 lib/modal.tsx 头注释）。
 * 开关与范围一律走 URL（§9.5 单一真源）：`?dialog=migrate-to-library&ids=1,2,3`
 *  - 深链/刷新/分享可直达同一次迁移；打开 push（back 关浮层）、关闭 replace（不制造历史垃圾），
 *    关掉时一并清掉 `ids`；
 *  - 一个能力一个入口：只此一处（图库工具条「迁入库」），不做独立页面、不做历史列表。
 *
 * 交互约定：
 *  - 源文件**默认保留**（copyin 只是复制，占双倍磁盘）；「迁完删源」要单独勾 + **二次确认**；
 *  - 只提交**可迁移**的项（reference + 未失联 + 有 fileHash）；其余在确认框里列出原因，
 *    不假装会迁（后端也会拦，但界面上先讲清楚）；
 *  - 执行后轮询 status 显示进度条，结束显示 成功/跳过（逐条原因）/失败。
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { useSearchParams } from 'react-router-dom'
import {
  api,
  formatSize,
  type AssetView,
  type MigrateDispatch,
  type MigrateStatus,
} from '../lib/api'
import { useCapabilities } from '../lib/caps'
import { Dialog, useOverlay } from '../lib/modal'
import { useToast } from '../lib/toast'
import { Button } from './Button'
import { parseIdsParam } from './ExportDialog'
import { tGlobal, useI18n } from '../i18n'

/**
 * 跳过原因人话（与后端 `svc_library::skip_reason_text` 同口径）。
 *
 * code 是稳定契约（前端只做映射，不自己发明原因）；未知 code 原样露出，不吞。
 */
export const MIGRATE_SKIP_TEXT: Record<string, string> = {
  not_found: '资产不存在或已删除',
  no_hash: '资产缺少内容指纹（fileHash）',
  already_copyin: '已是库内保管（copyin），无需迁移',
  missing_file: '磁盘上找不到原文件（失联）',
  content_changed: '磁盘文件内容与入库指纹不一致（被替换过）',
}

/** 跳过原因 code → 人话（未知 code 回显 code，便于排查）。 */
export function migrateSkipText(reason: string): string {
  const known = MIGRATE_SKIP_TEXT[reason]
  return known ? tGlobal(known) : tGlobal('跳过（{reason}）', { reason })
}

type Migratable = Pick<AssetView, 'storeMode' | 'missing' | 'fileHash'>

/**
 * 这一项能不能迁移（与后端 `skip_reason` 前四档同口径）。
 *
 * `content_changed` 要读盘算 sha256，前端判不了 —— 交给后端拦，成功后进 `skipped[]`。
 */
export function isMigratable(a: Migratable): boolean {
  return a.missing === 0 && !!a.fileHash && a.storeMode === 'reference'
}

/** 不能迁的原因（可迁 → 空串）；判据顺序与 `isMigratable` 一致。 */
export function migrateHoldReason(a: Migratable): string {
  if (a.missing !== 0) return tGlobal('失联（磁盘上找不到原文件）')
  if (!a.fileHash) return tGlobal('缺少内容指纹（fileHash）')
  if (a.storeMode !== 'reference') return tGlobal('已是库内保管（copyin）')
  return ''
}

function errText(e: unknown): string {
  return e instanceof Error ? e.message : String(e)
}

/** 不可迁移项最多列几条（其余折成「还有 N 张」）。 */
const HOLD_LIST_MAX = 10

export function MigrateToLibraryDialog({
  suspended = false,
  onFinished,
}: {
  /** 上层浮层（快捷键帮助）打开时挂起 Esc */
  suspended?: boolean
  /** 迁移跑完（有报告）时回调 —— 图库借此刷新列表（absPath/storeMode 已变） */
  onFinished?: () => void
}) {
  const { toast } = useToast()
  const ov = useOverlay('dialog', 'migrate-to-library', ['ids'])
  const { t } = useI18n()
  const [params] = useSearchParams()
  const ids = useMemo(() => parseIdsParam(params.get('ids')), [params])
  const idsKey = ids.join(',')
  const open = ov.open

  const caps = useCapabilities()
  /** 目标库根：capabilities.roots 里第一个可写根（服务端实测口径，前端不自己判形态） */
  const libraryRoot = useMemo(
    () => (caps?.roots ?? []).find((r) => r.writable)?.path ?? '',
    [caps],
  )

  const [assets, setAssets] = useState<AssetView[] | null>(null)
  const [loadErr, setLoadErr] = useState<string | null>(null)
  const [reloadTick, setReloadTick] = useState(0)
  const [deleteSource, setDeleteSource] = useState(false)
  /** 勾了「删源」后的二次确认（false = 还没确认，此时点开始只弹警告不提交） */
  const [needConfirmDelete, setNeedConfirmDelete] = useState(false)
  const [dispatch, setDispatch] = useState<MigrateDispatch | null>(null)
  const [status, setStatus] = useState<MigrateStatus | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [submitting, setSubmitting] = useState(false)
  const pollRef = useRef<number | null>(null)

  const stopPoll = useCallback(() => {
    if (pollRef.current !== null) {
      window.clearInterval(pollRef.current)
      pollRef.current = null
    }
  }, [])

  const startPoll = useCallback(
    (batchId: string) => {
      stopPoll()
      pollRef.current = window.setInterval(() => {
        void api
          .migrateToLibraryStatus(batchId)
          .then((s) => {
            setStatus(s)
            if (s.state === 'running') return
            stopPoll()
            if (s.state === 'idle') {
              // 后端快照是进程内的：重启/被清空后拿到 idle，不能装成「跑完了」
              setError(t('后端没有这次任务的进度记录（服务可能重启过），请重新发起'))
              return
            }
            const rep = s.report
            if (rep) {
              toast(
                rep.failed.length > 0 || rep.skipped.length > 0 ? 'warning' : 'success',
                t('迁入库完成：成功 {a} / {b}', { a: rep.migrated, b: rep.total }),
                [
                  rep.skipped.length > 0 ? t('跳过 {n}', { n: rep.skipped.length }) : '',
                  rep.failed.length > 0 ? t('失败 {n}', { n: rep.failed.length }) : '',
                  rep.bytes > 0 ? t('写入 {size}', { size: formatSize(rep.bytes) }) : '',
                ]
                  .filter(Boolean)
                  .join(' · '),
              )
              onFinished?.()
            } else if (s.error) {
              toast('error', t('迁入库失败'), s.error)
            }
          })
          .catch((e) => {
            stopPoll()
            setError(errText(e))
          })
      }, 400)
    },
    [stopPoll, toast, onFinished, t],
  )

  // 打开：复位本次状态 + 同步后端快照（对话框关着的时候任务可能还在跑）
  useEffect(() => {
    if (!open) return
    setError(null)
    setNeedConfirmDelete(false)
    setDeleteSource(false)
    setDispatch(null)
    setStatus(null)
    stopPoll()
    api
      .migrateToLibraryStatus()
      .then((s) => {
        if (s.state !== 'running') return
        setStatus(s)
        if (s.batchId) startPoll(s.batchId)
      })
      .catch(() => {
        /* 快照读不到不影响发起迁移（发起后照样轮询） */
      })
    return () => stopPoll()
  }, [open, stopPoll, startPoll])

  // 选中资产（判断哪些能迁）+ 重试
  useEffect(() => {
    if (!open) return
    if (ids.length === 0) {
      setAssets([])
      setLoadErr(null)
      return
    }
    let stop = false
    setAssets(null)
    setLoadErr(null)
    api
      .assets({ ids })
      .then((r) => {
        if (!stop) setAssets(r.items)
      })
      .catch((e) => {
        if (!stop) setLoadErr(errText(e))
      })
    return () => {
      stop = true
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps -- ids 由 idsKey 代表（数组每次渲染都是新引用）
  }, [open, idsKey, reloadTick])

  useEffect(() => stopPoll, [stopPoll])

  const migratableIds = useMemo(
    () => (assets ?? []).filter(isMigratable).map((a) => a.assetId),
    [assets],
  )
  const heldAssets = useMemo(() => (assets ?? []).filter((a) => !isMigratable(a)), [assets])

  const running = submitting || status?.state === 'running'
  const report = status?.report
  const failedBatch = status?.state === 'failed'

  /** 不能开始的原因（'' = 可以）；按钮禁用态与 tooltip 共用这一处判据。 */
  const blockedReason = useMemo(() => {
    if (ids.length === 0) return t('没有选择集：先在网格里选中要迁入库的图片')
    if (!caps) return t('正在读取库根…')
    if (!libraryRoot) return t('没有可写的库根：先在设置里配置库目录并确认可写')
    if (loadErr) return t('读取选中资产失败')
    if (assets === null) return t('正在读取选中资产…')
    if (migratableIds.length === 0) return t('选中项都无需迁移（已是库内保管 / 失联 / 缺内容指纹）')
    return ''
  }, [ids.length, caps, libraryRoot, loadErr, assets, migratableIds.length, t])

  const run = useCallback(async () => {
    setSubmitting(true)
    setNeedConfirmDelete(false)
    setError(null)
    setStatus(null)
    try {
      const d = await api.migrateToLibrary({ ids: migratableIds, deleteSource })
      setDispatch(d)
      setStatus({
        state: 'running',
        batchId: d.batchId,
        total: d.total,
        processed: 0,
        migrated: 0,
        skipped: 0,
        failed: 0,
        bytes: 0,
      })
      startPoll(d.batchId)
    } catch (e) {
      const msg = errText(e)
      setError(msg)
      toast('error', t('迁入库启动失败'), msg)
    } finally {
      setSubmitting(false)
    }
  }, [migratableIds, deleteSource, startPoll, toast, t])

  /**
   * 点「开始迁移」：
   *  - 已弹出删源二次确认（`needConfirmDelete`）→ 不再走这里（等用户点确认按钮）；
   *  - 勾了删源 → 先进入二次确认，不提交；
   *  - 没勾 → 直接跑。
   */
  const start = useCallback(() => {
    if (blockedReason || running || needConfirmDelete) return
    if (deleteSource) {
      setNeedConfirmDelete(true)
      return
    }
    void run()
  }, [blockedReason, running, needConfirmDelete, deleteSource, run])

  const total = status?.total ?? dispatch?.total ?? migratableIds.length
  const processed = status?.processed ?? 0
  const pct = total > 0 ? Math.min(100, Math.round((processed / total) * 100)) : 0
  const hasStatus = running || !!report || failedBatch || !!error

  return (
    <Dialog
      open={open}
      title={t('迁入库（{n} 张）', { n: ids.length })}
      subtitle={t('把原地索引（reference）的原图复制进库根并切为库内保管（copyin）。源文件默认保留。')}
      onClose={ov.closeOverlay}
      suspended={suspended}
      testId="migrate-dialog"
      widthClass="max-w-xl"
      footer={
        <>
          <Button
            type="submit"
            form="migrate-form"
            variant="primary"
            size="md"
            data-testid="migrate-start"
            disabled={!!blockedReason || running}
            title={blockedReason || t('开始迁移（Enter）')}
          >
            {submitting ? t('提交中…') : running ? t('迁移中…') : t('开始迁移（Enter）')}
          </Button>
          <Button type="button" onClick={ov.closeOverlay} variant="secondary" className="ml-auto">
            {running ? t('后台运行') : t('关闭')}
          </Button>
        </>
      }
    >
      <form
        id="migrate-form"
        data-testid="migrate-form"
        onSubmit={(e) => {
          e.preventDefault()
          start()
        }}
        className="space-y-3 text-xs"
      >
        {loadErr && (
          <div className="rounded border border-red-500/50 bg-red-500/10 p-2" data-testid="migrate-error">
            <p className="text-red-300">{t('读取选中资产失败：{err}', { err: loadErr })}</p>
            <Button
              type="button"
              className="mt-2"
              data-testid="migrate-reload"
              onClick={() => setReloadTick((n) => n + 1)}
            >
              {t('重试')}
            </Button>
          </div>
        )}

        {!loadErr && assets === null && ids.length > 0 && (
          <p className="text-ink-dim" data-testid="migrate-loading">
            {t('正在读取选中资产…')}
          </p>
        )}

        {!loadErr && assets !== null && (
          <div className="rounded border border-line bg-panel-2 p-3" data-testid="migrate-summary">
            <p className="break-all font-mono text-[11px] text-ink-dim">
              {t('目标库根：')}<span data-testid="migrate-root">{libraryRoot || t('（没有可写库根）')}</span>
            </p>
            <p className="mt-1.5">
              {t('可迁移')}{' '}
              <span className="font-semibold text-ink" data-testid="migrate-eligible">
                {migratableIds.length}
              </span>{' '}
              {t('张')}
              {heldAssets.length > 0 && (
                <>
                  {t(' · 不迁移')}{' '}
                  <span className="font-semibold text-ink" data-testid="migrate-held">
                    {heldAssets.length}
                  </span>{' '}
                  {t('张')}
                </>
              )}
            </p>
            {heldAssets.length > 0 && (
              <ul className="mt-1.5 space-y-0.5 text-[11px] text-amber-400" data-testid="migrate-held-list">
                {heldAssets.slice(0, HOLD_LIST_MAX).map((a) => (
                  <li key={a.assetId}>
                    #{a.assetId} {a.fileName} · {migrateHoldReason(a)}
                  </li>
                ))}
                {heldAssets.length > HOLD_LIST_MAX && (
                  <li>{t('还有 {n} 张（同类原因，不逐一列出）', { n: heldAssets.length - HOLD_LIST_MAX })}</li>
                )}
              </ul>
            )}
          </div>
        )}

        <label className="flex items-start gap-2">
          <input
            type="checkbox"
            checked={deleteSource}
            data-testid="migrate-delete-source"
            onChange={(e) => {
              setDeleteSource(e.target.checked)
              setNeedConfirmDelete(false)
            }}
            disabled={running}
            className="mt-0.5"
          />
          <span>
            {t('迁完删除源文件（不可撤销）')}
            <span className="mt-0.5 block text-[11px] text-ink-dim">
              {t('默认关闭：迁移只复制，源文件留在原处。勾选后提交前会再确认一次。')}
            </span>
          </span>
        </label>

        {needConfirmDelete && (
          <div
            className="rounded border border-red-500/60 bg-red-500/10 p-3"
            data-testid="migrate-delete-confirm"
          >
            <p className="text-red-300">
              {t('确认删除 {n} 个源文件？文件从原目录移除后无法恢复（库内会留一份副本）。', { n: migratableIds.length })}
            </p>
            <div className="mt-2 flex gap-2">
              <Button
                type="button"
                tone="danger"
                data-testid="migrate-delete-confirm-ok"
                onClick={() => void run()}
              >
                {t('确认删除源文件并迁移')}
              </Button>
              <Button
                type="button"
                data-testid="migrate-delete-confirm-cancel"
                onClick={() => setNeedConfirmDelete(false)}
              >
                {t('取消')}
              </Button>
            </div>
          </div>
        )}

        {hasStatus && (
          <div className="rounded border border-line bg-panel-2 p-3">
            {running && (
              <div data-testid="migrate-progress">
                <div className="flex items-center justify-between">
                  <span>
                    {t('迁移中 · {a}/{b}', { a: processed, b: total })}
                    {status?.skipped ? t(' · 跳过 {n}', { n: status.skipped }) : ''}
                    {status?.failed ? t(' · 失败 {n}', { n: status.failed }) : ''}
                  </span>
                  <span className="text-ink-dim">{pct}%</span>
                </div>
                <div className="mt-1.5 h-1.5 w-full overflow-hidden rounded bg-panel">
                  <div className="h-full bg-accent transition-all" style={{ width: `${pct}%` }} />
                </div>
              </div>
            )}

            {report && !running && (
              <div data-testid="migrate-report">
                <p className="text-sm">
                  {t('完成 · 成功')} <span className="font-semibold">{report.migrated}</span> / {report.total}
                  {report.skipped.length > 0 ? t(' · 跳过 {n}', { n: report.skipped.length }) : ''}
                  {report.failed.length > 0 ? t(' · 失败 {n}', { n: report.failed.length }) : ''}
                  {report.bytes > 0 ? t(' · 写入 {size}', { size: formatSize(report.bytes) }) : ''}
                </p>
                {report.skipped.length > 0 && (
                  <ul className="mt-1.5 space-y-0.5 text-[11px] text-amber-400" data-testid="migrate-skipped">
                    {report.skipped.map((s) => (
                      <li key={s.assetId}>
                        #{s.assetId} · {migrateSkipText(s.reason)}
                        {s.detail && s.detail !== migrateSkipText(s.reason) ? ` · ${s.detail}` : ''}
                      </li>
                    ))}
                  </ul>
                )}
                {report.failed.length > 0 && (
                  <ul className="mt-1.5 space-y-0.5 text-[11px] text-red-400" data-testid="migrate-failed">
                    {report.failed.map((f) => (
                      <li key={f.assetId}>
                        #{f.assetId} · {f.code} · {f.detail}
                      </li>
                    ))}
                  </ul>
                )}
                {report.warnings.length > 0 && (
                  <p className="mt-1.5 text-[11px] text-amber-400" data-testid="migrate-warnings">
                    {t('告警 {n} 条：{detail}', { n: report.warnings.length, detail: report.warnings[0].detail })}
                  </p>
                )}
              </div>
            )}

            {failedBatch && (
              <p className="text-red-300" data-testid="migrate-batch-failed">
                {t('整批失败：{reason}', { reason: status?.error ?? t('未知原因') })}
              </p>
            )}

            {error && <p className="mt-1.5 text-red-300" data-testid="migrate-error">{error}</p>}

            {status?.state === 'idle' && !error && <p className="text-ink-dim">{t('没有进行中的迁移任务。')}</p>}
          </div>
        )}
      </form>
    </Dialog>
  )
}
