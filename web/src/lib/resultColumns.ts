/**
 * 结果表的列口径（N5b）：**列由插件 schema 声明，宿主通用渲染**。
 *
 * 权威：`damdesk-product/references/plugin-result-contract.md` / 协议 §4.5。
 * 三条硬规则（这一层就是它们的实现）：
 * 1. 外壳固定、内容自由：执行结果 `result` 三值、审核结论 `verdict.status` 三态永远在；
 *    行业语义只能进 `fields`，宿主按 key 取，不认识也不报错。
 * 2. 列由插件声明（`result_columns`），宿主只按 schema 画表格/排序/筛选/导出；
 *    **未声明 → 默认列**（资产 / 执行结果 / 产物 / 备注），judge 才追加审核列。
 * 3. 坏声明不许让整页报错：非法列丢掉并给 warnings，全非法 = 按未声明处理。
 *
 * 与 Rust `pack_host::result_columns` 同一套闭集；两边改一处，前端测试会红。
 */

import { getGlobalLocale } from '../i18n'
import { pickI18n, type PackText } from '../i18n/pickI18n'
import {
  resultLabels,
  verdictLabels,
  hasJudge,
  type PluginOutput,
  type ResultStatus,
  type VerdictStatus,
} from './pluginResult'
import type { ArtifactFileView } from './api'
import { tGlobal } from '../i18n'

/** 列类型闭集（自造类型 = 前端不可渲染）。 */
export const COLUMN_TYPES = ['number', 'text', 'enum', 'bool'] as const
export type ColumnType = (typeof COLUMN_TYPES)[number]

/** 插件声明的列（`resources/result_columns.json` / API `columns`）。 */
export interface ResultColumnDecl {
  key: string
  /** 表头展示串（后端已按请求语言取好；老字段恒为字符串） */
  label: string
  /** 表头原样 i18n 对象（i18n-F；包写对象形态时才有，前端 pickI18n 切语言即时生效） */
  labelI18n?: PackText
  type: ColumnType
  unit?: string | null
  order?: number
}

/** 渲染用列：固定列 / 行业列 / 审核列（来源不同，样式与可排序性一致）。 */
export type TableColumnKind = 'fixed' | 'industry' | 'audit'

export interface TableColumn {
  key: string
  label: string
  /** 表头原样 i18n 对象（i18n-F；同 ResultColumnDecl 口径） */
  labelI18n?: PackText
  type: ColumnType
  unit?: string | null
  kind: TableColumnKind
}

/** 结果表的一行（`GET /api/compliance/results` 的 item）。 */
export interface ResultRowView {
  assetId: number
  fileName?: string | null
  ruleId?: string | null
  judgementScope?: string | null
  /** 执行结果（所有行都有） */
  result: ResultStatus
  /** 审核结论（只有判定类行有；处理类行 = null） */
  verdict?: {
    status: VerdictStatus
    confidence?: number | null
    reasonCode?: string | null
  } | null
  /** 人读原因（执行失败/跳过/判定原因） */
  reason?: string | null
  /** 行业自定义列的数据（插件原样给，宿主当不透明字典） */
  fields?: Record<string, unknown> | null
  /**
   * 产物（处理类行 = `ArtifactFileView[]`（export_file 契约）；老数据/历史行可能是
   * 裸路径字符串数组 —— 两种形态都能画，产物列渲染与导出统一走 [`artifactFiles`]）。
   */
  artifacts?: (string | ArtifactFileView)[] | null
  ts?: number
}

/** 产物条目 → 文件路径（裸字符串原样；对象取 `path`；没有 → null） */
export function artifactPath(a: string | ArtifactFileView): string | null {
  if (typeof a === 'string') return a || null
  return typeof a.path === 'string' && a.path ? a.path : null
}

/** 产物条目 → 文件名（basename；给产物列/CSV 用，不把整段目录路径糊进表格） */
export function artifactBasename(a: string | ArtifactFileView): string | null {
  const p = artifactPath(a)
  if (p == null) return null
  const i = Math.max(p.lastIndexOf('/'), p.lastIndexOf('\\'))
  return i >= 0 ? p.slice(i + 1) : p
}

/** 一行的产物文件列表（同形态归一；空 → []） */
export function artifactFiles(row: ResultRowView): (string | ArtifactFileView)[] {
  return (row.artifacts ?? []).filter((a) => artifactPath(a) != null)
}

const isColumnType = (v: unknown): v is ColumnType =>
  typeof v === 'string' && (COLUMN_TYPES as readonly string[]).includes(v)

/**
 * 归一化声明：丢非法项并给原因（`order` 升序、同 order 保持声明顺序）。
 * 与 Rust 侧 `parse_result_columns` 同口径 —— 这是**前端兜底**（老后端/手改包也不炸）。
 */
export function normalizeColumns(raw: unknown): {
  columns: ResultColumnDecl[] | null
  warnings: string[]
} {
  if (raw == null) return { columns: null, warnings: [] }
  const arr = Array.isArray(raw)
    ? raw
    : Array.isArray((raw as { result_columns?: unknown }).result_columns)
      ? ((raw as { result_columns: unknown[] }).result_columns as unknown[])
      : Array.isArray((raw as { columns?: unknown }).columns)
        ? ((raw as { columns: unknown[] }).columns as unknown[])
        : null
  if (!arr) return { columns: null, warnings: [tGlobal('result_columns 必须是数组')] }

  const out: ResultColumnDecl[] = []
  const warnings: string[] = []
  arr.forEach((item, i) => {
    if (item == null || typeof item !== 'object') {
      warnings.push(tGlobal('第 {n} 项不是对象，已忽略', { n: i + 1 }))
      return
    }
    const raw = item as Record<string, unknown>
    const key = typeof raw.key === 'string' ? raw.key.trim() : ''
    if (!key) {
      warnings.push(tGlobal('第 {n} 项缺 key，已忽略', { n: i + 1 }))
      return
    }
    if (out.some((c) => c.key === key)) {
      warnings.push(tGlobal('key {key} 重复声明，保留第一条', { key }))
      return
    }
    if (!isColumnType(raw.type)) {
      warnings.push(
        tGlobal('{key} 的 type {type} 不在闭集（{types}），已忽略', {
          key,
          type: String(raw.type),
          types: COLUMN_TYPES.join('/'),
        }),
      )
      return
    }
    // i18n-F：label 兼容对象形态（{"locale": 文案}）——按当前语言取值，原样对象进 labelI18n
    const labelI18n =
      raw.label != null && typeof raw.label === 'object' && !Array.isArray(raw.label)
        ? (raw.label as PackText)
        : typeof raw.labelI18n === 'object' && raw.labelI18n != null
          ? (raw.labelI18n as PackText)
          : undefined
    const label =
      typeof raw.label === 'string' && raw.label.trim()
        ? raw.label.trim()
        : pickI18n(labelI18n, getGlobalLocale()) || key
    out.push({
      key,
      label,
      labelI18n,
      type: raw.type,
      unit: typeof raw.unit === 'string' && raw.unit.trim() ? raw.unit.trim() : undefined,
      order: typeof raw.order === 'number' && Number.isFinite(raw.order) ? raw.order : 0,
    })
  })
  if (!out.length) {
    if (!warnings.length) warnings.push(tGlobal('result_columns 为空：按未声明处理（默认列）'))
    return { columns: null, warnings }
  }
  // 稳定排序：order 升序，同 order 保持声明顺序
  const sorted = out
    .map((c, i) => ({ c, i }))
    .sort((a, b) => (a.c.order ?? 0) - (b.c.order ?? 0) || a.i - b.i)
    .map((x) => x.c)
  return { columns: sorted, warnings }
}

/** i18n-D：固定列 / 默认列 / 审核列的 label 每次调用取词（模块级常量会固化译文）。 */
const fixedLeading = (): TableColumn[] => [
  { key: '__asset', label: tGlobal('资产'), type: 'text', kind: 'fixed' },
  { key: '__result', label: tGlobal('执行结果'), type: 'enum', kind: 'fixed' },
]

/** 未声明 `result_columns` 时的默认列（协议 §4.5 原文）。 */
const defaultTrailing = (): TableColumn[] => [
  { key: '__artifacts', label: tGlobal('产物'), type: 'text', kind: 'fixed' },
  { key: '__note', label: tGlobal('备注'), type: 'text', kind: 'fixed' },
]

/** judge 插件追加的审核列（处理类**没有**审核面，这是红线）。 */
const auditColumns = (): TableColumn[] => [
  { key: '__verdict', label: tGlobal('状态'), type: 'enum', kind: 'audit' },
  { key: '__reason', label: tGlobal('原因'), type: 'text', kind: 'audit' },
  { key: '__confidence', label: tGlobal('置信度'), type: 'number', kind: 'audit' },
]

/**
 * 表格的最终列序：
 * `[资产, 执行结果] + (声明列 | [产物, 备注]) + (judge ? [状态, 原因, 置信度] : [])`
 *
 * PROC-RES-2：处理类（outputs 含 process）**恒有产物列** —— 插件声明了行业列时也把
 * `__artifacts` 追加在声明列之后（产物是处理结果的本体，不能被列声明挤掉）。
 */
export function tableColumns(
  declared: ResultColumnDecl[] | null | undefined,
  outputs: readonly PluginOutput[] | undefined | null,
): TableColumn[] {
  const industry: TableColumn[] = (declared ?? []).map((c) => ({
    key: c.key,
    label: c.label,
    labelI18n: c.labelI18n,
    type: c.type,
    unit: c.unit,
    kind: 'industry',
  }))
  let middle = industry.length ? industry : defaultTrailing()
  if ((outputs ?? []).includes('artifact') && !middle.some((c) => c.key === '__artifacts')) {
    middle = [...middle, { key: '__artifacts', label: tGlobal('产物'), type: 'text', kind: 'fixed' }]
  }
  return [...fixedLeading(), ...middle, ...(hasJudge(outputs) ? auditColumns() : [])]
}

/** 单元格取值（展示用字符串；数字/布尔原值给排序/导出用）。 */
export function cellValue(col: TableColumn, row: ResultRowView): string | number | boolean | null {
  switch (col.key) {
    case '__asset':
      return row.fileName ? `${row.fileName}` : String(row.assetId)
    case '__result':
      return resultLabels()[row.result] ?? row.result
    case '__artifacts': {
      const names = artifactFiles(row)
        .map((a) => artifactBasename(a) ?? '')
        .filter(Boolean)
      return names.length ? names.join(tGlobal('；')) : '—'
    }
    case '__note':
      return row.reason ?? ''
    case '__verdict':
      return row.verdict ? (verdictLabels()[row.verdict.status] ?? row.verdict.status) : '—'
    case '__reason':
      return row.verdict?.reasonCode ?? row.reason ?? ''
    case '__confidence':
      return row.verdict?.confidence ?? null
    default: {
      const v = row.fields?.[col.key]
      if (v == null) return null
      if (Array.isArray(v)) return v.join('、')
      if (typeof v === 'object') return JSON.stringify(v)
      return v as string | number | boolean
    }
  }
}

/** 展示文本（数字带单位，布尔说人话，空 = `—`）。 */
export function cellText(col: TableColumn, row: ResultRowView): string {
  const v = cellValue(col, row)
  if (v == null || v === '') return '—'
  if (typeof v === 'boolean') return v ? tGlobal('是') : tGlobal('否')
  if (typeof v === 'number') {
    if (col.key === '__confidence') return v.toFixed(2)
    return col.unit ? `${v}${col.unit}` : String(v)
  }
  return String(v)
}

export type SortDir = 'asc' | 'desc'

/** 排序：数字列按值、布尔按真假、其余按中文词序；空值永远殿后。 */
export function sortRows(
  rows: readonly ResultRowView[],
  col: TableColumn,
  dir: SortDir = 'asc',
): ResultRowView[] {
  const factor = dir === 'asc' ? 1 : -1
  const rank = (r: ResultRowView) => {
    const v = cellValue(col, r)
    return v == null || v === '' ? 1 : 0
  }
  return [...rows].sort((a, b) => {
    const ra = rank(a)
    const rb = rank(b)
    if (ra !== rb) return ra - rb // 空值殿后，不随方向翻转
    const va = cellValue(col, a)
    const vb = cellValue(col, b)
    if (typeof va === 'number' || typeof vb === 'number') {
      return (Number(va ?? 0) - Number(vb ?? 0)) * factor
    }
    if (typeof va === 'boolean' || typeof vb === 'boolean') {
      return (Number(Boolean(va)) - Number(Boolean(vb))) * factor
    }
    return String(va ?? '').localeCompare(String(vb ?? ''), 'zh-Hans-CN') * factor
  })
}

/** 每列的关键词过滤（大小写不敏感子串；空串 = 不过滤）。 */
export function filterRows(
  rows: readonly ResultRowView[],
  cols: readonly TableColumn[],
  filters: Record<string, string>,
): ResultRowView[] {
  const active = cols
    .map((c) => [c, (filters[c.key] ?? '').trim().toLowerCase()] as const)
    .filter(([, q]) => q.length > 0)
  if (!active.length) return [...rows]
  return rows.filter((row) =>
    active.every(([col, q]) => cellText(col, row).toLowerCase().includes(q)),
  )
}

/** 导出 CSV（带 BOM，Excel 直接开；列序 = 当前表格列序）。 */
export function toCsv(cols: readonly TableColumn[], rows: readonly ResultRowView[]): string {
  const esc = (s: string) => (/[",\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s)
  const header = cols.map((c) => esc(c.unit ? `${c.label}(${c.unit})` : c.label)).join(',')
  const lines = rows.map((r) => cols.map((c) => esc(cellText(c, r))).join(','))
  return `\uFEFF${[header, ...lines].join('\n')}\n`
}
