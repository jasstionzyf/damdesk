/**
 * 插件结果行契约（N5a，前端侧类型 + 闭集）。
 *
 * 权威：`damdesk-product/references/plugin-result-contract.md` / 协议 §4.4。
 * 与 Rust `crates/pack-host/src/result.rs`、Python `dampack/protocol.py` 同一套闭集 ——
 * 三边抄错任何一边，`pluginResult.test.ts` / `test_protocol_parity.py` 会红。
 *
 * 两条硬边界（不在这一层做渲染，渲染在 N5b 的 `result_columns`）：
 * 1. **执行结果 ≠ 判定结论**：`result` 人人都有，`verdict` 只有判定类有；
 * 2. 自造状态一律不可渲染：`result` 三值、`verdict.status` 三态，其它语义只能进
 *    `reasonCode` / `fields` / `evidence`。
 */
import { tGlobal } from '../i18n'

/** 插件种类（manifest `outputs`）：宿主据此决定有没有审核列，不靠猜。 */
export const PLUGIN_KINDS = ['verdict', 'artifact'] as const
export type PluginOutput = (typeof PLUGIN_KINDS)[number]

/** 执行结果三值（所有插件必填）。 */
export const RESULT_STATUSES = ['ok', 'failed', 'skipped'] as const
export type ResultStatus = (typeof RESULT_STATUSES)[number]

/** 审核结论三态（只有判定类；`skipped` 是执行结果，不是结论）。 */
export const VERDICT_STATUSES = ['pass', 'fail', 'undetermined'] as const
export type VerdictStatus = (typeof VERDICT_STATUSES)[number]

/** 产物只准两种去处：资产工作版本 / 导出文件（带 sha256）。 */
export const ARTIFACT_KINDS = ['working_version', 'export_file'] as const
export type ArtifactKind = (typeof ARTIFACT_KINDS)[number]

export interface WorkingVersionArtifact {
  kind: 'working_version'
  assetId: string
  versionId: string
}

export interface ExportFileArtifact {
  kind: 'export_file'
  path: string
  sha256: string
}

export type ResultArtifact = WorkingVersionArtifact | ExportFileArtifact

/** 判定块（只有 `outputs` 含 verdict 才允许出现）。 */
export interface RowVerdict {
  status: VerdictStatus
  /** 把握度 0..1；缺失 = 包内未给（宿主不拿 0 冒充把握度） */
  confidence?: number
  /** 机读原因码（如 `R3_no_watermark`），展示与筛选都靠它 */
  reasonCode?: string
}

/** 执行失败原因（`result=failed` 必带）。 */
export interface RowError {
  code: string
  message: string
}

/** 一行结果（行键 = assetId + ruleId；判定类按规则出行，处理类按资产出行）。 */
export interface ResultRow {
  assetId: string | number
  ruleId?: string
  result: ResultStatus
  verdict?: RowVerdict
  error?: RowError
  skipReason?: string
  artifacts?: ResultArtifact[]
  /** 自由结构（bbox/OCR/模型回复/耗时/参数）：只展示，不参与逻辑 */
  evidence?: Record<string, unknown>
  /** 行业自定义列：宿主当不透明字典渲染 */
  fields?: Record<string, unknown>
}

/** `outputs` 是否含判定（决定要不要审核列/审核动作）。 */
export function hasJudge(outputs: readonly PluginOutput[] | undefined | null): boolean {
  return (outputs ?? []).includes('verdict')
}

/** `outputs` 是否含处理。 */
export function hasProcess(outputs: readonly PluginOutput[] | undefined | null): boolean {
  return (outputs ?? []).includes('artifact')
}

/**
 * 一行是「审核结论」还是「纯执行结果」——
 * 处理类插件的每个结果行都只有执行结果，前端不得给它画审核状态。
 */
export function isJudgedRow(row: ResultRow, outputs: readonly PluginOutput[]): boolean {
  return hasJudge(outputs) && !!row.verdict
}

/** 结果行的展示标签（自造状态在前端直接原样显示 = 造出用户看不懂的状态，所以走闭集）。
 *  i18n-D：函数取词（模块级常量会固化译文）；`RESULT_LABELS` 常量保留原文键形态供既有
 *  消费方过渡（C 卡批量迁移时统一切 resultLabels()）。 */
export function resultLabels(): Record<ResultStatus, string> {
  return {
    ok: tGlobal('已执行'),
    failed: tGlobal('执行失败'),
    skipped: tGlobal('已跳过'),
  }
}

/** 原文键常量（i18n-D 过渡期保留；locale 感知取词走 resultLabels()） */
export const RESULT_LABELS: Record<ResultStatus, string> = {
  ok: '已执行',
  failed: '执行失败',
  skipped: '已跳过',
}

/** 审核状态标签（判定类才用；用户面命名表 §3：通过 / 未通过 / 待人工看）。 */
export function verdictLabels(): Record<VerdictStatus, string> {
  return {
    pass: tGlobal('通过'),
    fail: tGlobal('未通过'),
    undetermined: tGlobal('待人工看'),
  }
}

/** 原文键常量（i18n-D 过渡期保留；locale 感知取词走 verdictLabels()） */
export const VERDICT_LABELS: Record<VerdictStatus, string> = {
  pass: '通过',
  fail: '未通过',
  undetermined: '待人工看',
}

/** 行级备注：判定原因 → 执行失败原因 → 跳过原因（三选一，别混成一句话）。 */
export function rowNote(row: ResultRow): string {
  if (row.verdict) return row.verdict.reasonCode ?? verdictLabels()[row.verdict.status]
  if (row.error) return `${row.error.code}：${row.error.message}`
  if (row.skipReason) return row.skipReason
  return ''
}
