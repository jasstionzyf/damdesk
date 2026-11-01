/**
 * 「仅检查重复（不入库）」—— 库外文件夹只读查重 Dialog（KI-7 / 卡 t_8f8a23c2）。
 *
 * 定位：**导入前的一步**。用户手上有一个库外文件夹，只想先知道里面有没有重复，
 * 不想把它导进库。所以：
 *  - 入口挂在「＋ 导入」下拉第二项与命令面板（`?dialog=dedup-scan` 可深链）；
 *  - 受能力位 `features.scanExternalPath` 门控：容器/网页形态（服务端的「本地路径」
 *    是服务器磁盘）**直接不渲染**，连 URL 深链进来也只给一句话原因，不放路径输入框；
 *  - 结果只读：能看、能导出报告、能定位文件；**没有删除入口**（库外没有资产记录可软删）。
 *
 * 判据与服务端同一套（SHA-256 精确 ∪ pHash-D4 八变体汉明），界面不出现模型名/向量字样。
 */
import { useState } from 'react'
import { api, formatSize, type DupScanReport } from '../lib/api'
import { can, reasonFor, useCapabilities } from '../lib/caps'
import { Dialog, useOverlay } from '../lib/modal'
import { isDesktopShell, pickFolder } from '../lib/native'
import { useToast } from '../lib/toast'
import { useI18n } from '../i18n'

const btnPrimary =
  'rounded bg-accent px-3 py-1.5 text-xs font-medium text-white disabled:opacity-50'
const btnGhost =
  'rounded border border-line px-3 py-1.5 text-xs text-ink hover:border-accent hover:text-accent disabled:opacity-50'

function Stat({ label, value }: { label: string; value: string | number }) {
  return (
    <div className="rounded-md border border-line bg-panel-2 px-3 py-2">
      <div className="text-xs text-ink-dim">{label}</div>
      <div className="text-base font-semibold">{value}</div>
    </div>
  )
}

/** 报告正文（纯展示，便于 SSR 单测断言文案） */
export function ScanReportView({ report }: { report: DupScanReport }) {
  const { t } = useI18n()
  return (
    <div className="space-y-3" data-testid="scan-report">
      <div className="grid grid-cols-3 gap-2">
        <Stat label={t('扫描文件')} value={report.scanned} />
        <Stat label={t('重复组')} value={report.totalGroups} />
        <Stat label={t('可释放')} value={formatSize(report.reclaimBytes)} />
      </div>
      <p className="text-xs text-ink-dim" data-testid="scan-note">
        {report.note}
        {report.unreadable.length > 0 && t(' · {n} 个文件读不了（跳过）', { n: report.unreadable.length })}
      </p>
      {report.totalGroups === 0 ? (
        <p className="rounded-md border border-line bg-panel-2 px-3 py-2 text-xs text-ink-dim">
          {t('这个文件夹里没有发现重复图（同文件或近似重复）。')}
        </p>
      ) : (
        <div className="space-y-2">
          {report.groups.map((g) => (
            <div
              key={g.groupId}
              className="rounded-md border border-line bg-panel-2 p-2"
              data-testid={`scan-group-${g.groupId}`}
            >
              <div className="flex items-center gap-2 text-xs">
                <span
                  className={`rounded px-1.5 py-0.5 text-[10px] font-semibold ${
                    g.kind === 'exact'
                      ? 'bg-red-500/15 text-red-400'
                      : 'bg-sky-500/15 text-sky-400'
                  }`}
                >
                  {g.kind === 'exact' ? t('精确重复（同文件）') : t('近似重复')}
                </span>
                <span className="text-ink-dim">
                  {t('组 {id} · {n} 张 · 可释放 {size}', {
                    id: g.groupId,
                    n: g.size,
                    size: formatSize(g.reclaimBytes),
                  })}
                </span>
              </div>
              <ul className="mt-1 space-y-0.5">
                {g.members.map((m) => (
                  <li
                    key={m.path}
                    className="flex items-baseline gap-2 text-[11px] leading-4"
                    title={m.path}
                  >
                    <span
                      className={`w-10 shrink-0 ${m.keep ? 'text-emerald-400' : 'text-ink-dim'}`}
                    >
                      {m.keep ? t('保留') : t('重复')}
                    </span>
                    <span className="min-w-0 flex-1 truncate">{m.fileName}</span>
                    <span className="shrink-0 text-ink-dim">{formatSize(m.fileSz)}</span>
                    {m.relation && (
                      <span className="shrink-0 text-ink-dim/80">{m.relation}</span>
                    )}
                  </li>
                ))}
              </ul>
              {g.evidence.length > 0 && (
                <p className="mt-1 text-[10px] text-ink-dim/80">{t('证据：{e}', { e: g.evidence.join(t('；')) })}</p>
              )}
            </div>
          ))}
        </div>
      )}
    </div>
  )
}

export function DedupScanDialog() {
  const ov = useOverlay('dialog', 'dedup-scan')
  const caps = useCapabilities()
  const { toast } = useToast()
  const { t } = useI18n()
  const [path, setPath] = useState('')
  const [busy, setBusy] = useState(false)
  const [report, setReport] = useState<DupScanReport | null>(null)
  const [error, setError] = useState('')

  const allowed = can(caps, 'scanExternalPath')
  if (!ov.open) return null

  const run = async () => {
    const p = path.trim()
    if (!p) {
      setError(t('先选一个文件夹（或直接粘贴绝对路径）'))
      return
    }
    setBusy(true)
    setError('')
    try {
      const r = await api.dedupScan(p)
      setReport(r.report)
    } catch (e) {
      setError(String(e instanceof Error ? e.message : e))
      setReport(null)
    } finally {
      setBusy(false)
    }
  }

  const browse = async () => {
    const picked = await pickFolder(t('选择要查重的文件夹（不会导入）'))
    if (picked) setPath(picked)
    else if (!isDesktopShell()) {
      toast('info', t('当前形态没有系统文件夹选择框'), t('可以直接粘贴文件夹绝对路径'))
    }
  }

  return (
    <Dialog
      open={ov.open}
      title={t('仅检查重复（不入库）')}
      subtitle={t('只读扫描库外文件夹：算文件指纹和图像指纹，给出重复分组；不会导入、不会改动任何文件。')}
      onClose={ov.closeOverlay}
      testId="dedup-scan-dialog"
      widthClass="max-w-2xl"
      footer={
        allowed && report ? (
          <>
            <a
              href={api.dedupExportUrl({ format: 'csv', scope: `scan:${report.scanId}` })}
              download
              className={btnGhost}
              data-testid="scan-export"
            >
              {t('导出报告（CSV）')}
            </a>
            <button type="button" className={btnGhost} onClick={ov.closeOverlay}>
              {t('关闭')}
            </button>
          </>
        ) : (
          <button type="button" className={btnGhost} onClick={ov.closeOverlay}>
            {t('关闭')}
          </button>
        )
      }
    >
      {!allowed ? (
        <p className="text-sm text-ink-dim" data-testid="scan-disabled">
          {reasonFor(caps, 'scanExternalPath') ||
            t('当前形态读不到你电脑上的文件夹（文件在服务器上），请在桌面版使用。')}
        </p>
      ) : (
        <div className="space-y-3">
          <div className="flex items-end gap-2">
            <label className="min-w-0 flex-1 text-xs">
              <span className="mb-1 block text-ink-dim">{t('库外文件夹（绝对路径）')}</span>
              <input
                type="text"
                value={path}
                onChange={(e) => setPath(e.target.value)}
                placeholder={t('例如 D:\\待整理\\2024 或 /Users/me/Pictures')}
                data-testid="scan-path"
                className="w-full rounded border border-line bg-panel-2 px-2 py-1.5 font-mono text-xs text-ink"
              />
            </label>
            <button type="button" className={btnGhost} onClick={() => void browse()} data-testid="scan-browse">
              {t('选择文件夹…')}
            </button>
            <button
              type="button"
              className={btnPrimary}
              disabled={busy}
              onClick={() => void run()}
              data-testid="scan-run"
            >
              {busy ? t('扫描中…') : t('开始扫描')}
            </button>
          </div>
          <p className="text-[11px] text-ink-dim">
            {t('按扩展名收集目录内图片（含子目录），逐个比对文件指纹与图像指纹；大目录会分批扫描并提示剩余数量。')}
          </p>
          {error && (
            <p className="rounded-md border border-red-600/40 bg-red-500/10 px-3 py-2 text-xs text-red-300" data-testid="scan-error">
              {error}
            </p>
          )}
          {report && <ScanReportView report={report} />}
        </div>
      )}
    </Dialog>
  )
}
