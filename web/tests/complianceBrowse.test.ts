/**
 * USABILITY-AUDIT（卡 task）前端单测：合规页「按状态浏览」的判定摘要口径。
 *
 * 只测纯函数（不发请求）：角标只收 pass/fail/undetermined，`skipped`（占位/未执行）
 * 不得被渲染成结论 —— 合规页一旦把「跳过」当结论展示，用户看到的就是假的判定结果。
 */
import { describe, expect, it } from 'vitest'
import { verdictSummary } from '../src/lib/complianceBrowse'
import type { ComplianceVerdictView } from '../src/lib/api'

function row(ruleId: string, result: string): ComplianceVerdictView {
  return {
    ruleId,
    ruleName: `规则 ${ruleId}`,
    result,
    score: null,
    reason: null,
    evidence: null,
    fixAction: null,
    fixTier: null,
    recheckResult: null,
    ts: 1,
    feedback: null,
  }
}

describe('verdictSummary: 只有真结论进角标', () => {
  it('pass/fail/undetermined 计入，skipped 不计', () => {
    const s = verdictSummary([row('R1', 'pass'), row('R2', 'skipped'), row('R3', 'fail')])
    expect(s.total).toBe(2)
    expect(s.shown.map((r) => r.ruleId)).toEqual(['R1', 'R3'])
    expect(s.rest).toBe(0)
  })

  it('超过 max 条时折叠成 +N', () => {
    const rows = [row('R1', 'pass'), row('R2', 'fail'), row('R3', 'undetermined'), row('R4', 'pass')]
    const s = verdictSummary(rows, 2)
    expect(s.total).toBe(4)
    expect(s.shown).toHaveLength(2)
    expect(s.rest).toBe(2)
  })

  it('全是 skipped / 空数组 → 0 条结论（卡片显示「无判定行」而不是角标）', () => {
    expect(verdictSummary([])).toEqual({ shown: [], total: 0, rest: 0 })
    expect(verdictSummary([row('R1', 'skipped')]).total).toBe(0)
  })

  it('未知 result 一律不当作结论（不猜）', () => {
    expect(verdictSummary([row('R1', 'error')]).total).toBe(0)
  })
})
