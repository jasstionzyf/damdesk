/**
 * COMPLIANCE-UI3：三态判定口径的**前端唯一口径**（纯函数，node 单测覆盖）。
 *
 * 用户裁决（2026-10-17）：机器只给三档结论 —— 合格 / 不合格 / 未判定，
 * 「未判定」**只表示判定过程出错**（读图失败、VL 超时/不可解析、插件异常），
 * 不是「机器不敢判、等人来判」。人工改判只覆盖**生效结论**（finalStatus），
 * 机器结论（machineResult + machineConfidence）永远原样保留、界面必须能同时显示。
 *
 * 本模块只做「词表/档位/排序/分组/改判门槛」这些纯计算，不碰网络；
 * 页面与检查器都从这里取口径，避免两处各写一套徽章语义。
 */
import type { ComplianceStateWord, ComplianceStates, HumanReviewView } from './api'
import { tGlobal } from '../i18n'
import type { PackText } from '../i18n/pickI18n'

/** 界面只认的三个档位（顺序 = 「要处理的」在前） */
export type TierWord = 'pass' | 'fail' | 'undetermined'

export const TIER_ORDER: TierWord[] = ['fail', 'undetermined', 'pass']

/** i18n-D：档位标签每次调用按当前 locale 取词（模块级常量会固化译文）。
 *  `TIER_LABELS` 常量保留原文键形态供既有消费方过渡（C 卡批量迁移时统一切 tierLabels()）。 */
export function tierLabels(): Record<TierWord, string> {
  return {
    pass: tGlobal('合格'),
    fail: tGlobal('不合格'),
    undetermined: tGlobal('未判定'),
  }
}

/** 原文键常量（i18n-D 过渡期保留；locale 感知取词走 tierLabels()） */
export const TIER_LABELS: Record<TierWord, string> = {
  pass: '合格',
  fail: '不合格',
  undetermined: '未判定',
}

/** 档位 → 一句话语义（统计栏/徽章 title 共用，禁止各处另写解释） */
export function tierSemantics(): Record<TierWord, string> {
  return {
    pass: tGlobal('所有规则都给出了合格结论（含修复后复检通过）'),
    fail: tGlobal('至少一条规则判不合格（不合格压过未判定）'),
    undetermined: tGlobal('检查过程没跑出结论（读图失败 / 模型超时或不可解析 / 执行器异常），不是人工审批'),
  }
}

/** 原文键常量（i18n-D 过渡期保留；locale 感知取词走 tierSemantics()） */
export const TIER_SEMANTICS: Record<TierWord, string> = {
  pass: '所有规则都给出了合格结论（含修复后复检通过）',
  fail: '至少一条规则判不合格（不合格压过未判定）',
  undetermined: '检查过程没跑出结论（读图失败 / 模型超时或不可解析 / 执行器异常），不是人工审批',
}

/** 旧词 → 三态（0016 迁移前的历史行/旧 CSV 仍可能出现，读侧兼容） */
const LEGACY_WORDS: Record<string, string> = {
  fixed: 'pass',
  repaired: 'pass',
  pending_human: 'undetermined',
  failed: 'undetermined',
}

/** skipped = 该规则对这张图没有结论（依赖短路/打标型/作用域不符），既不参与也不污染 */
export const SKIPPED = 'skipped'

export function normalizeState(result: string): string {
  return LEGACY_WORDS[result] ?? result
}

export function stateLabel(result: string): string {
  const w = normalizeState(result)
  if (w === 'pass') return tGlobal('合格')
  if (w === 'fail') return tGlobal('不合格')
  if (w === 'undetermined') return tGlobal('未判定')
  if (w === 'unjudged') return tGlobal('未判定（未跑）')
  if (w === SKIPPED) return tGlobal('无结论')
  return result
}

export type Tone = 'ok' | 'bad' | 'unknown' | 'skip'

export function stateTone(result: string): Tone {
  const w = normalizeState(result)
  if (w === 'pass') return 'ok'
  if (w === 'fail') return 'bad'
  if (w === 'undetermined' || w === 'unjudged') return 'unknown'
  return 'skip'
}

/** 徽章 title：三档语义 + 「未判定」与「待人工」的界线（消除旧口径歧义） */
export function stateSemantics(result: string): string {
  const w = normalizeState(result)
  if (w === 'pass' || w === 'fail' || w === 'undetermined') return tierSemantics()[w]
  if (w === 'unjudged') return tGlobal('当前方案下这张图还没检查过（不是不合格，也不是过程出错）')
  if (w === SKIPPED) return tGlobal('该规则对这张图不适用（依赖短路/打标型/作用域不符），不计入结论')
  return ''
}

// ---------------------------------------------------------------- 统计三档

export interface TierCounts {
  /** 三档展示值（合计 == 总数；未判定 = 过程出错 + 未跑判定） */
  pass: number
  fail: number
  undetermined: number
  /** 未判定里「过程出错」与「未跑判定」的明细（明细只作说明，不参与合计） */
  processError: number
  notJudged: number
  /** 参与判定的资产数（= total - notJudged 也只是近似；total 才是全库分母） */
  judged: number
  total: number
}

/** 后端分桶 → 三档（未判定把 undetermined 与 unjudged 合并，合计必须等于 total） */
export function tiersOf(states: ComplianceStates | null | undefined): TierCounts | null {
  if (!states) return null
  const processError = states.undetermined ?? 0
  const notJudged = states.unjudged ?? 0
  const pass = states.pass ?? 0
  const fail = states.fail ?? 0
  return {
    pass,
    fail,
    undetermined: processError + notJudged,
    processError,
    notJudged,
    judged: states.judged ?? pass + fail + processError,
    total: states.total ?? pass + fail + processError + notJudged,
  }
}

/** 三档合计（单测守这条不变量：界面说「合计 = 总数」就不能少算一档） */
export function tierSum(t: TierCounts): number {
  return t.pass + t.fail + t.undetermined
}

// ---------------------------------------------------------------- 置信度 / 改判门槛

/** 置信度显示（两位小数；null = 0016 前的历史行，明确说「未知」而不是 0 或 1） */
export function confidenceText(c: number | null | undefined): string {
  if (c == null || !Number.isFinite(c)) return tGlobal('置信度未知')
  return tGlobal('置信度 {v}', { v: (Math.round(c * 100) / 100).toFixed(2) })
}

/**
 * 是否需要「高置信度确认」（服务端校验同口径：confidence >= threshold 时改判要 force）。
 * 历史行（confidence = null）按「需要复核」处理 → 免 force。
 */
export function needsForce(confidence: number | null | undefined, threshold: number): boolean {
  if (confidence == null || !Number.isFinite(confidence)) return false
  return confidence >= threshold
}

/** 置信度未知的历史行要显式说明，别让用户以为机器很确定 */
export function confidenceHint(confidence: number | null | undefined, threshold: number): string {
  if (confidence == null || !Number.isFinite(confidence)) {
    return tGlobal('这条是历史判定行（写入置信度之前），置信度不可知 —— 一律进复核')
  }
  if (confidence >= threshold) {
    return tGlobal('置信度 {c} ≥ 阈值 {t}：改判需勾选确认', { c: confidence.toFixed(2), t: threshold.toFixed(2) })
  }
  return tGlobal('置信度 {c} < 阈值 {t}：机器没把握，可直接改判', { c: confidence.toFixed(2), t: threshold.toFixed(2) })
}

// ---------------------------------------------------------------- 复核行（列表/队列同源）

/** 复核行：`GET /api/compliance/review` 的扁平项（一个 (asset, rule) 一行） */
export interface ReviewRow {
  assetId: number
  fileName: string
  absPath: string
  thumbPath?: string | null
  /** null = 资产级改判（整张图一条结论） */
  ruleId?: string | null
  ruleName?: string | null
  /** 规则名原样 i18n 对象（i18n-F；包写对象形态时才有） */
  ruleNameI18n?: PackText
  judgementScope?: string | null
  /** 机器结论（pass / fail / undetermined / skipped；资产级 = 机器汇总档位） */
  machineResult: string
  machineConfidence: number | null
  machineScore?: number | null
  /** 机器为何得不出结论（undetermined 才有） */
  reason?: string | null
  lowConfidence: boolean
  /** 生效结论 = humanStatus ?? machineResult */
  finalStatus: string
  humanReview?: HumanReviewView | null
  reviewed: boolean
  ts: number
}

/** 改判说明：「机器判不合格(0.42) → 人工合格」；无改判返回空串（界面不显示假改判） */
export function overrideNote(row: Pick<ReviewRow, 'machineResult' | 'machineConfidence' | 'humanReview'>): string {
  const h = row.humanReview
  if (!h) return ''
  const conf = row.machineConfidence == null ? '' : `(${row.machineConfidence.toFixed(2)})`
  return tGlobal('机器判{machine}{conf} → 人工改判{human}', {
    machine: stateLabel(row.machineResult),
    conf,
    human: stateLabel(h.humanStatus),
  })
}

/** 生效结论与机器结论不同 = 有改判覆盖（不是「已处置」那种软标记） */
export function overridden(row: Pick<ReviewRow, 'machineResult' | 'humanReview'>): boolean {
  if (!row.humanReview) return false
  return normalizeState(row.humanReview.humanStatus) !== normalizeState(row.machineResult)
}

export type SortKey = 'confidence' | 'fileName' | 'path' | 'asset'

/** i18n-D：排序选项标签每次调用按当前 locale 取词 */
export function sortLabels(): Record<SortKey, string> {
  return {
    confidence: tGlobal('机器置信度（最没把握的先看）'),
    fileName: tGlobal('文件名'),
    path: tGlobal('路径'),
    asset: tGlobal('资产号'),
  }
}

/** 原文键常量（i18n-D 过渡期保留；locale 感知取词走 sortLabels()） */
export const SORT_LABELS: Record<SortKey, string> = {
  confidence: '机器置信度（最没把握的先看）',
  fileName: '文件名',
  path: '路径',
  asset: '资产号',
}

/**
 * 行排序。置信度：升序、null（不可知）最前 —— 「最没把握的先看」；
 * 其余键按 localeCompare（中文文件名不走 pinjyin 也稳定），同键时资产号升序兜底。
 * 服务端只支持 asset/confidence 两种序，这里统一在前端做（列表/队列两视图共用）。
 */
export function sortRows<T extends ReviewRow>(rows: T[], key: SortKey): T[] {
  const out = [...rows]
  const byAsset = (a: T, b: T) => a.assetId - b.assetId
  const byRule = (a: T, b: T) => String(a.ruleId ?? '').localeCompare(String(b.ruleId ?? ''))
  if (key === 'confidence') {
    out.sort((a, b) => {
      const av = a.machineConfidence ?? -1
      const bv = b.machineConfidence ?? -1
      if (av !== bv) return av - bv
      return byAsset(a, b) || byRule(a, b)
    })
  } else if (key === 'fileName') {
    out.sort((a, b) => a.fileName.localeCompare(b.fileName, 'zh-CN') || byAsset(a, b) || byRule(a, b))
  } else if (key === 'path') {
    out.sort((a, b) => a.absPath.localeCompare(b.absPath, 'zh-CN') || byAsset(a, b) || byRule(a, b))
  } else {
    out.sort((a, b) => byAsset(a, b) || byRule(a, b))
  }
  return out
}

export interface AssetGroup<T extends ReviewRow> {
  assetId: number
  fileName: string
  absPath: string
  thumbPath: string | null
  rows: T[]
  /** 组内任一行的生效结论（口径：不合格 > 未判定 > 合格；资产级行优先） */
  worst: TierWord
  /** 组内有生效改判 */
  reviewed: boolean
}

/** 队列视图 = 同一批扁平行按资产聚合（第一层资产 → 第二层规则） */
export function groupByAsset<T extends ReviewRow>(rows: T[]): AssetGroup<T>[] {
  const map = new Map<number, AssetGroup<T>>()
  for (const r of rows) {
    let g = map.get(r.assetId)
    if (!g) {
      g = {
        assetId: r.assetId,
        fileName: r.fileName,
        absPath: r.absPath,
        thumbPath: r.thumbPath ?? null,
        rows: [],
        worst: 'pass',
        reviewed: false,
      }
      map.set(r.assetId, g)
    }
    g.rows.push(r)
    if (r.reviewed) g.reviewed = true
  }
  for (const g of map.values()) g.worst = worstTier(g.rows.map((r) => r.finalStatus))
  return [...map.values()]
}

const TIER_RANK: Record<TierWord, number> = { fail: 0, undetermined: 1, pass: 2 }

/** 组内最差档位（不合格 > 未判定 > 合格）；空集按合格（不该发生，宁可不算差） */
export function worstTier(results: string[]): TierWord {
  let worst: TierWord = 'pass'
  for (const r of results) {
    const w = normalizeState(r)
    const tier: TierWord = w === 'fail' ? 'fail' : w === 'undetermined' || w === 'unjudged' ? 'undetermined' : 'pass'
    if (TIER_RANK[tier] < TIER_RANK[worst]) worst = tier
  }
  return worst
}

/** 行是否需要复核（服务端口径：undetermined 或 confidence < 阈值；历史 NULL 也算） */
export function needsReview(row: Pick<ReviewRow, 'machineResult' | 'machineConfidence'>, threshold: number): boolean {
  if (normalizeState(row.machineResult) === 'undetermined') return true
  if (normalizeState(row.machineResult) === SKIPPED) return false
  return row.machineConfidence == null ? true : row.machineConfidence < threshold
}

/** 队列/列表分组后的行数（诚实计数：界面上的「N 项」= 行数，不是资产数） */
export function rowCount(groups: { rows: unknown[] }[]): number {
  return groups.reduce((n, g) => n + g.rows.length, 0)
}

/**
 * IP-K09：判定失败稳定词 → 人话（复核队列 Dialog 用；原在合规页，页面下线后搬到 lib 单一实现）。
 *
 * **词来自后端，别在界面发明**：处理插件的失败词由 `svc-pipeline::PluginFailure::reason()` 固定
 * （报告/筛选器按它们聚合）；历史行里还可能有迁移前内联引擎留下的 rule_error / load_error /
 * confidence_missing，读侧只做翻译不新增。界面只做翻译：括号里保留原词，方便对日志与报告。
 */
export const REASON_LABELS: Record<string, string> = {
  plugin_timeout: '检查超时（没在时限内应答）',
  plugin_crash: '检查中断（重启后仍失败 → 冷却）',
  plugin_unavailable: '检查能力不可用（未安装 / 冷却中 / 解释器缺失）',
  plugin_rule_error: '规则级报错',
  plugin_protocol_error: '结果不合协议（结论不采信）',
  plugin_confidence: '未给置信度',
  rule_error: '规则执行报错',
  load_error: '读图 / 装载失败',
  confidence_missing: '缺置信度',
  group_key_missing: '组键缺失（组级判定不可用）',
}

/** 稳定原因词 → 人话（无可译词时原样返回；`null`/空串返回 null，界面据此不渲染原因行） */
export function reasonText(reason: string | null | undefined): string | null {
  if (!reason) return null
  const hit = REASON_LABELS[reason]
  return hit ? tGlobal('{hit}（{reason}）', { hit: tGlobal(hit), reason }) : reason
}

/** 插件通道的稳定原因词一律 `plugin_*` —— 这是「这行由子进程插件产出的」唯一的行级硬证据 */
export function isPluginReason(reason: string | null | undefined): boolean {
  return typeof reason === 'string' && reason.startsWith('plugin_')
}

/** 档位徽章样式（Tailwind 类；与任务中心结果面共用一套色语义） */
export function tierClass(result: string): string {
  const tone = stateTone(result)
  if (tone === 'ok') return 'border-emerald-600/60 bg-emerald-600/10 text-emerald-300'
  if (tone === 'bad') return 'border-rose-600/60 bg-rose-600/10 text-rose-300'
  if (tone === 'unknown') return 'border-amber-600/60 bg-amber-600/10 text-amber-300'
  return 'border-line bg-panel-2 text-ink-dim'
}

/** 合规状态词（库视图 filter 用）稳定词校验：只认后端三态 + 未跑判定 */
export const FILTER_WORDS: ComplianceStateWord[] = ['pass', 'fail', 'undetermined', 'unjudged']
