/**
 * 通用结果表（N5b）：插件声明 `result_columns`，宿主只按 schema 画表格/排序/筛选/导出。
 *
 * 三条不变量（结果行契约，见 `lib/resultColumns.ts`）：
 * 1. 执行结果（ok/failed/skipped）人人都有；审核列（状态/原因/置信度）**只有 judge 插件**才有，
 *    处理类插件不画审核面 —— 那是「把批处理冒充审核」的信任红线；
 * 2. 未声明列 → 默认列（资产 / 执行结果 / 产物 / 备注），不报错；
 * 3. 行业列只从 `fields[key]` 取值，宿主不认识也照原样显示（不猜语义、不做映射）。
 *
 * 排序/筛选/导出都用**同一份列定义**：三处口径分裂就会出现「界面按 A 排、导出按 B 排」。
 */
import { useEffect, useMemo, useState } from 'react'
import { useI18n } from '../i18n'

import { ArtifactPreview } from './ArtifactPreview'
import { Button } from './Button'
import { humanRevealError } from './PathLink'
import { can, reasonFor, useCapabilities } from '../lib/caps'
import { useArtifactDownload } from '../lib/artifactActions'
import { useToast } from '../lib/toast'
import { pickI18nOr, usePackTextLocale } from '../i18n/pickI18n'
import {
  filterRows,
  sortRows,
  tableColumns,
  cellText,
  toCsv,
  artifactBasename,
  artifactFiles,
  artifactPath,
  type ResultColumnDecl,
  type ResultRowView,
  type SortDir,
  type TableColumn,
} from '../lib/resultColumns'
import { api, artifactUrl } from '../lib/api'
import type { PluginOutput } from '../lib/pluginResult'

export interface ResultTableProps {
  rows: readonly ResultRowView[]
  /** 插件声明的列（null/undefined = 未声明 → 默认列） */
  columns?: ResultColumnDecl[] | null
  /** 安装记录里的插件种类（judge 才画审核列） */
  outputs?: readonly PluginOutput[] | null
  /** 总命中数（服务端 total；缺省 = rows.length） */
  total?: number
  loading?: boolean
  error?: string | null
  /** 列声明被丢弃的项与原因（可折叠提示，不影响表格） */
  columnWarnings?: string[]
  /**
   * PROC-RES-2：行可勾选（处理类结果表用 —— 批量收集产物导出）。
   * 勾选状态在表内维护，变化时通过 `onSelectedRowsChange` 上报**当前筛选下被勾中的行**。
   */
  selectable?: boolean
  onSelectedRowsChange?: (rows: ResultRowView[]) => void
  /**
   * DM-B2（KI-37）：行首加一列缩略图 —— 「判的是图就得看见图」。
   * 点缩略图交给上层（任务中心 = 打开图列表 Dialog）。
   */
  thumbnails?: boolean
  onThumbClick?: (row: ResultRowView) => void
  /** 测试/埋点用前缀 */
  testid?: string
}

function headerClass(col: TableColumn): string {
  const base = 'whitespace-nowrap px-2 py-1 text-left text-[11px] font-medium '
  if (col.kind === 'audit') return `${base}text-accent-soft`
  if (col.kind === 'industry') return `${base}text-ink`
  return `${base}text-ink-dim`
}

export function ResultTable({
  rows,
  columns = null,
  outputs = null,
  total,
  loading = false,
  error = null,
  columnWarnings = [],
  selectable = false,
  onSelectedRowsChange,
  thumbnails = false,
  onThumbClick,
  testid = 'result-table',
}: ResultTableProps) {
  const { t } = useI18n()
  // i18n-F：表头按当前语言取包自带列名（labelI18n 命中优先），切语言即时生效；
  // CSV 导出走同一份 cols，导出表头与界面一致。
  const locale = usePackTextLocale()
  const cols = useMemo(
    () =>
      tableColumns(columns, outputs).map((c) => ({
        ...c,
        label: pickI18nOr(c.labelI18n, c.label, locale),
      })),
    [columns, outputs, locale],
  )
  const [sortKey, setSortKey] = useState<string | null>(null)
  const [sortDir, setSortDir] = useState<SortDir>('asc')
  const [filters, setFilters] = useState<Record<string, string>>({})
  /** 行 key：同资产同规则可能多行（多次执行），用序号兜底保证唯一 */
  const rowKey = (row: ResultRowView, i: number) => `${row.assetId}-${row.ruleId ?? '-'}-${i}`
  const [selected, setSelected] = useState<Set<string>>(new Set())

  // PROC-RES-3：产物预览浮层（表内本地状态，不进 URL —— 它是「看一次就走」的检视动作，
  // 不是可深链的页面；关掉后表里的筛选/滚动位置原样保留）
  const [preview, setPreview] = useState<{ path: string; name: string } | null>(null)
  const [revealing, setRevealing] = useState<string | null>(null)
  const onDownloadClick = useArtifactDownload()
  const { toast } = useToast()
  const caps = useCapabilities()
  const canReveal = can(caps, 'openInOS')
  const revealReason = reasonFor(caps, 'openInOS')

  /** 「在文件夹中显示」：走唯一接口 + 唯一人话映射（PathLink 的 `humanRevealError`） */
  const revealArtifact = async (p: string, name: string) => {
    setRevealing(p)
    try {
      const r = await api.osReveal(p, 'reveal')
      toast('success', r.label || t('已在文件夹中显示'), `${name} —— ${r.path}`)
    } catch (e) {
      toast('error', t('没能打开文件夹'), humanRevealError(e, revealReason))
    } finally {
      setRevealing(null)
    }
  }

  const shown = useMemo(() => {
    const filtered = filterRows(rows, cols, filters)
    const col = cols.find((c) => c.key === sortKey)
    return col ? sortRows(filtered, col, sortDir) : filtered
  }, [rows, cols, filters, sortKey, sortDir])

  // 换一次执行（rows 换了整套）→ 勾选清零，不跨执行残留（已是空集就不动，防无谓重渲染）
  useEffect(() => {
    setSelected((prev) => (prev.size === 0 ? prev : new Set()))
  }, [rows])

  const selectedRows = useMemo(
    () => shown.filter((r, i) => selected.has(rowKey(r, i))),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [shown, selected],
  )
  useEffect(() => {
    onSelectedRowsChange?.(selectedRows)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedRows])

  const allShownSelected = selectable && shown.length > 0 && selectedRows.length === shown.length
  const toggleAll = () => {
    setSelected((prev) => {
      const next = new Set(prev)
      if (allShownSelected) shown.forEach((r, i) => next.delete(rowKey(r, i)))
      else shown.forEach((r, i) => next.add(rowKey(r, i)))
      return next
    })
  }
  const toggleRow = (key: string) => {
    setSelected((prev) => {
      const next = new Set(prev)
      if (next.has(key)) next.delete(key)
      else next.add(key)
      return next
    })
  }

  const toggleSort = (key: string) => {
    if (sortKey === key) setSortDir((d) => (d === 'asc' ? 'desc' : 'asc'))
    else {
      setSortKey(key)
      setSortDir('asc')
    }
  }

  const exportCsv = () => {
    const csv = toCsv(cols, shown)
    const blob = new Blob([csv], { type: 'text/csv;charset=utf-8' })
    const url = URL.createObjectURL(blob)
    const a = document.createElement('a')
    a.href = url
    a.download = 'compliance-results.csv'
    a.click()
    URL.revokeObjectURL(url)
  }

  const judge = cols.some((c) => c.kind === 'audit')
  const filtering = Object.values(filters).some((v) => v.trim() !== '')

  /**
   * 产物单元格（PROC-RES-2 / PROC-RES-3）：缩略图（产物本体，CSS 限宽）+ 文件名 +
   * 打开（**应用内预览**，不再新开窗口）+ 下载（去重 + toast + 失败判定）+ 在文件夹中显示。
   *
   * 「打开」仍是 `<a href>`（右键/复制链接的老习惯不变），但点击被本组件接管去开预览浮层 ——
   * 新开标签页在桌面壳里等于把产物甩给浏览器，用户离开应用就再也回不到这次执行的结果。
   */
  const artifactCell = (row: ResultRowView) => {
    const files = artifactFiles(row)
    if (!files.length) return <span>—</span>
    return (
      <span className="flex flex-col gap-1">
        {files.map((a, idx) => {
          const p = artifactPath(a)!
          const name = artifactBasename(a) ?? p
          return (
            <span key={p} className="flex items-center gap-1.5" data-testid={`${testid}-artifact-${row.assetId}-${idx}`}>
              <img
                src={artifactUrl(p)}
                alt={name}
                loading="lazy"
                title={t('点击预览产物')}
                onClick={() => setPreview({ path: p, name })}
                className="h-9 w-9 shrink-0 cursor-zoom-in rounded border border-line object-cover"
                data-testid={`${testid}-artifact-thumb-${row.assetId}-${idx}`}
              />
              <span className="max-w-40 truncate font-mono text-[11px]" title={p}>
                {name}
              </span>
              <a
                href={artifactUrl(p)}
                onClick={(e) => {
                  e.preventDefault()
                  setPreview({ path: p, name })
                }}
                className="text-accent hover:underline"
                data-testid={`${testid}-artifact-open-${row.assetId}-${idx}`}
              >
                {t('打开')}
              </a>
              <a
                href={artifactUrl(p, true)}
                onClick={(e) => onDownloadClick(e, p, name)}
                className="text-accent hover:underline"
                data-testid={`${testid}-artifact-download-${row.assetId}-${idx}`}
              >
                {t('下载')}
              </a>
              {/* 容器形态（openInOS=false）不渲染按钮：灰按钮能点会被骂，人话原因在预览浮层的 PathLink 里说 */}
              {canReveal && (
                <button
                  type="button"
                  title={`${name} —— ${p}`}
                  disabled={revealing === p}
                  onClick={() => void revealArtifact(p, name)}
                  className="whitespace-nowrap text-accent hover:underline disabled:opacity-50"
                  data-testid={`${testid}-artifact-reveal-${row.assetId}-${idx}`}
                >
                  {t('在文件夹中显示')}
                </button>
              )}
            </span>
          )
        })}
      </span>
    )
  }

  return (
    <div className="flex min-h-0 flex-col gap-2" data-testid={testid}>
      <div className="flex flex-wrap items-center gap-2 text-[11px] text-ink-dim">
        <span data-testid={`${testid}-count`}>
          {t('命中')} <span className="tabular-nums text-ink">{total ?? rows.length}</span>
          {filtering && shown.length !== rows.length && (
            <span className="ml-1">{t('（已筛出 {n}）', { n: shown.length })}</span>
          )}
        </span>
        <span data-testid={`${testid}-outputs`}>
          {t('审核列：{v}', { v: judge ? t('有（检查类结果）') : t('无（处理类结果不产生审核结论）') })}
        </span>
        <span data-testid={`${testid}-columns-source`}>
          {t('列来源：{v}', { v: columns && columns.length ? t('能力声明的列') : t('默认列') })}
        </span>
        <span className="flex-1" />
        <Button type="button" variant="secondary" onClick={exportCsv} data-testid={`${testid}-export`}>
          {t('导出 CSV')}
        </Button>
      </div>

      {error && (
        <div className="rounded border border-rose-600/60 bg-rose-600/10 px-2 py-1 text-xs text-rose-300" data-testid={`${testid}-error`}>
          {t('结果表读取失败：{err}', { err: error })}
        </div>
      )}

      {columnWarnings.length > 0 && (
        <details className="rounded border border-amber-600/50 bg-amber-600/10 px-2 py-1 text-[11px] text-amber-300">
          <summary className="cursor-pointer" data-testid={`${testid}-warnings`}>
            {t('列声明有 {n} 项被忽略（表格按可用列渲染）', { n: columnWarnings.length })}
          </summary>
          <ul className="mt-1 list-disc pl-4">
            {columnWarnings.map((w) => (
              <li key={w}>{w}</li>
            ))}
          </ul>
        </details>
      )}

      <div className="min-h-0 overflow-auto rounded border border-line">
        <table className="w-full border-collapse text-xs">
          <thead className="sticky top-0 z-10 bg-panel-2">
            <tr>
              {selectable && (
                <th
                  scope="col"
                  className="w-8 px-2 py-1 text-left text-[11px] font-medium text-ink-dim"
                  data-testid={`${testid}-th-__select`}
                >
                  <input
                    type="checkbox"
                    checked={allShownSelected}
                    onChange={toggleAll}
                    aria-label={t('全选本页结果行')}
                    data-testid={`${testid}-select-all`}
                    className="cursor-pointer accent-[var(--color-accent,#c8f)]"
                  />
                </th>
              )}
              {thumbnails && (
                <th
                  scope="col"
                  className="w-14 px-2 py-1 text-left text-[11px] font-medium text-ink-dim"
                  data-testid={`${testid}-th-__thumb`}
                >
                  {t('图')}
                </th>
              )}
              {cols.map((col) => {
                const active = sortKey === col.key
                return (
                  <th
                    key={col.key}
                    scope="col"
                    className={headerClass(col)}
                    aria-sort={active ? (sortDir === 'asc' ? 'ascending' : 'descending') : 'none'}
                    data-testid={`${testid}-th-${col.key}`}
                  >
                    <button
                      type="button"
                      onClick={() => toggleSort(col.key)}
                      title={t('按「{col}」排序', { col: col.label })}
                      className="inline-flex cursor-pointer items-center gap-1 hover:text-ink focus-visible:ring-2 focus-visible:ring-accent"
                      data-testid={`${testid}-sort-${col.key}`}
                    >
                      {col.label}
                      {col.unit ? <span className="opacity-70">({col.unit})</span> : null}
                      <span className="text-[9px] opacity-70">{active ? (sortDir === 'asc' ? '▲' : '▼') : '⇅'}</span>
                    </button>
                  </th>
                )
              })}
            </tr>
            <tr>
              {selectable && <th className="px-1 pb-1" />}
              {thumbnails && <th className="px-1 pb-1" />}
              {cols.map((col) => (
                <th key={col.key} className="px-1 pb-1">
                  <input
                    value={filters[col.key] ?? ''}
                    onChange={(e) => setFilters((f) => ({ ...f, [col.key]: e.target.value }))}
                    placeholder={t('筛选')}
                    aria-label={t('筛选 {col}', { col: col.label })}
                    data-testid={`${testid}-filter-${col.key}`}
                    className="w-full rounded border border-line bg-panel px-1 py-0.5 text-[11px] text-ink placeholder:text-ink-dim/60 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-accent"
                  />
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {shown.map((row, i) => (
              <tr
                key={rowKey(row, i)}
                className="border-t border-line/60 hover:bg-panel-2"
                data-testid={`${testid}-row-${row.assetId}-${row.ruleId ?? 'asset'}`}
              >
                {selectable && (
                  <td className="px-2 py-1 align-top" data-testid={`${testid}-cell-${row.assetId}-__select`}>
                    <input
                      type="checkbox"
                      checked={selected.has(rowKey(row, i))}
                      onChange={() => toggleRow(rowKey(row, i))}
                      aria-label={t('选择 {name} 这一行', { name: row.fileName ?? row.assetId })}
                      data-testid={`${testid}-select-row-${row.assetId}`}
                      className="cursor-pointer accent-[var(--color-accent,#c8f)]"
                    />
                  </td>
                )}
                {thumbnails && (
                  <td className="px-2 py-1 align-top" data-testid={`${testid}-cell-${row.assetId}-__thumb`}>
                    <button
                      type="button"
                      onClick={() => onThumbClick?.(row)}
                      className="block h-10 w-10 overflow-hidden rounded border border-line hover:border-accent/60"
                      title={onThumbClick ? t('看这批图') : (row.fileName ?? undefined)}
                      data-testid={`${testid}-thumb-${row.assetId}`}
                    >
                      <img
                        src={api.thumbUrl(row.assetId, 256)}
                        alt={row.fileName ?? `#${row.assetId}`}
                        loading="lazy"
                        className="h-full w-full object-cover"
                      />
                    </button>
                  </td>
                )}
                {cols.map((col) => (
                  <td
                    key={col.key}
                    className={`px-2 py-1 align-top ${col.kind === 'industry' ? 'text-ink' : 'text-ink-dim'}`}
                    data-testid={`${testid}-cell-${row.assetId}-${col.key}`}
                  >
                    {col.key === '__artifacts' && artifactFiles(row).length
                      ? artifactCell(row)
                      : cellText(col, row)}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
        {!shown.length && (
          <div className="px-3 py-6 text-center text-xs text-ink-dim" data-testid={`${testid}-empty`}>
            {loading
              ? t('读取中…')
              : rows.length
                ? t('当前筛选无命中')
                : t('还没有结果：先去图库选图「送去处理」，或在检查页开始一次检查')}
          </div>
        )}

        {/* PROC-RES-3：产物预览（Esc / 点遮罩 / ✕ 关闭，回表格不丢筛选与滚动） */}
        <ArtifactPreview
          path={preview?.path ?? null}
          name={preview?.name ?? null}
          onClose={() => setPreview(null)}
          testid={`${testid}-artifact-preview`}
        />
      </div>
    </div>
  )
}
