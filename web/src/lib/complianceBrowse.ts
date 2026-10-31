import type { ComplianceVerdictView } from './api'

/**
 * 合规页「按状态浏览」共用纯函数（卡 task / USABILITY-AUDIT P0-1）。
 *
 * 放在 lib 里是为了能在 node 环境单测（本仓 web 测试不带 jsdom 兜底渲染，
 * 纯函数是最稳的断言面）。
 */

/** 「有结论」的判定行（skipped/跳过占位行不算结论，不能拿去当角标骗人；三态口径 pass/fail/undetermined） */
const JUDGED = ['pass', 'fail', 'undetermined'] as const

export interface VerdictSummary {
  /** 前 max 条结论（给卡片角标） */
  shown: ComplianceVerdictView[]
  /** 结论总数 */
  total: number
  /** 被折叠掉的条数（卡片显示「+N」） */
  rest: number
}

/**
 * 判定摘要：只把 pass/fail/undetermined 计入角标。`skipped`（未执行/占位行）
 * 与其它结果不算结论 —— 否则「跳过」会被渲染成一条像结论的角标。
 */
export function verdictSummary(rows: ComplianceVerdictView[], max = 3): VerdictSummary {
  const judged = rows.filter((r) => (JUDGED as readonly string[]).includes(r.result))
  return { shown: judged.slice(0, max), total: judged.length, rest: Math.max(0, judged.length - max) }
}
