/**
 * COMPLIANCE-UI3 前端口径单测（`web/src/lib/compliance.ts`）。
 *
 * 只测**纯函数**（不发请求、不渲染）：三档合计不变量 / 旧词映射 / 徽章语义 /
 * 改判门槛 / 排序 / 资产两级聚合 / 覆盖说明。真实链路由
 * `scripts/ui3_compliance_acceptance.sh`（真库 + 真 HTTP + 真点击）覆盖。
 */
import { describe, expect, it } from 'vitest'
import {
  confidenceHint,
  confidenceText,
  groupByAsset,
  needsForce,
  needsReview,
  normalizeState,
  overridden,
  overrideNote,
  rowCount,
  sortRows,
  stateLabel,
  stateSemantics,
  stateTone,
  tierClass,
  tiersOf,
  tierSum,
  worstTier,
  type ReviewRow,
} from '../src/lib/compliance'
import type { ComplianceStates } from '../src/lib/api'

function states(over: Partial<ComplianceStates> = {}): ComplianceStates {
  return { pass: 10, fail: 3, undetermined: 2, unjudged: 5, judged: 15, total: 20, ...over }
}

function row(over: Partial<ReviewRow> = {}): ReviewRow {
  return {
    assetId: 1,
    fileName: 'a.jpg',
    absPath: '/data/a.jpg',
    thumbPath: null,
    ruleId: 'F1',
    ruleName: '白边',
    machineResult: 'pass',
    machineConfidence: 0.4,
    lowConfidence: true,
    finalStatus: 'pass',
    humanReview: null,
    reviewed: false,
    ts: 100,
    ...over,
  }
}

describe('三档统计：未判定 = 过程出错 + 未跑判定，三档合计 == 总数', () => {
  it('合并不判定两桶且合计等于 total', () => {
    const t = tiersOf(states())!
    expect(t.undetermined).toBe(7) // 2 + 5
    expect(t.processError).toBe(2)
    expect(t.notJudged).toBe(5)
    expect(tierSum(t)).toBe(20)
    expect(tierSum(t)).toBe(t.total)
  })

  it('空/未加载不造数', () => {
    expect(tiersOf(null)).toBeNull()
    expect(tiersOf(undefined)).toBeNull()
  })

  it('全 0 也满足不变量（不因缺字段算出负数）', () => {
    const t = tiersOf({ pass: 0, fail: 0, undetermined: 0, unjudged: 0, judged: 0, total: 0 })!
    expect(tierSum(t)).toBe(0)
  })
})

describe('三态词表：只认 pass/fail/undetermined(/unjudged)，旧词读侧映射', () => {
  it('旧词映射（0016 迁移前的行/旧 CSV 不能变成空档）', () => {
    expect(normalizeState('fixed')).toBe('pass')
    expect(normalizeState('repaired')).toBe('pass')
    expect(normalizeState('pending_human')).toBe('undetermined')
    expect(normalizeState('failed')).toBe('undetermined')
    expect(normalizeState('fail')).toBe('fail')
  })

  it('中文标签：未判定 ≠ 待人工（口径必须说清）', () => {
    expect(stateLabel('pass')).toBe('合格')
    expect(stateLabel('fail')).toBe('不合格')
    expect(stateLabel('undetermined')).toBe('未判定')
    expect(stateLabel('unjudged')).toBe('未判定（未跑）')
    expect(stateLabel('skipped')).toBe('无结论')
    expect(stateLabel('pending_human')).toBe('未判定')
  })

  it('色调/样式：未判定是「不知道」而不是「坏」', () => {
    expect(stateTone('pass')).toBe('ok')
    expect(stateTone('fail')).toBe('bad')
    expect(stateTone('undetermined')).toBe('unknown')
    expect(stateTone('unjudged')).toBe('unknown')
    expect(stateTone('skipped')).toBe('skip')
    expect(tierClass('undetermined')).toContain('amber')
    expect(tierClass('fail')).toContain('rose')
  })

  it('徽章语义说明必须显式否认旧「等人批」口径', () => {
    expect(stateSemantics('undetermined')).toContain('过程')
    expect(stateSemantics('undetermined')).toContain('不是人工审批')
    // N12：用户视角的动词是「检查」（判定是引擎口径，不出现在界面上）
    expect(stateSemantics('unjudged')).toContain('还没检查过')
    expect(stateSemantics('skipped')).toContain('不适用')
  })
})

describe('置信度与改判门槛', () => {
  it('null = 历史行「未知」，绝不显示 0/1', () => {
    expect(confidenceText(null)).toBe('置信度未知')
    expect(confidenceText(undefined)).toBe('置信度未知')
    expect(confidenceText(0.42)).toBe('置信度 0.42')
    expect(confidenceText(1)).toBe('置信度 1.00')
  })

  it('高置信度（>= 阈值）改判要 force；历史 NULL 免 force', () => {
    expect(needsForce(0.9, 0.7)).toBe(true)
    expect(needsForce(0.7, 0.7)).toBe(true)
    expect(needsForce(0.69, 0.7)).toBe(false)
    expect(needsForce(null, 0.7)).toBe(false)
  })

  it('提示文案区分「历史行」与「高置信度」', () => {
    expect(confidenceHint(null, 0.7)).toContain('历史判定行')
    expect(confidenceHint(0.9, 0.7)).toContain('改判需勾选确认')
    expect(confidenceHint(0.2, 0.7)).toContain('可直接改判')
  })

  it('复核口径：undetermined 必进；skipped 不进；NULL 置信度进', () => {
    expect(needsReview({ machineResult: 'undetermined', machineConfidence: 0.99 }, 0.7)).toBe(true)
    expect(needsReview({ machineResult: 'skipped', machineConfidence: null }, 0.7)).toBe(false)
    expect(needsReview({ machineResult: 'pass', machineConfidence: null }, 0.7)).toBe(true)
    expect(needsReview({ machineResult: 'pass', machineConfidence: 0.8 }, 0.7)).toBe(false)
  })
})

describe('机器结论 vs 生效结论（改判不改写机器）', () => {
  const review = {
    id: 1,
    ruleId: 'F1',
    scope: 'per_asset',
    humanStatus: 'pass' as const,
    reason: '误报',
    at: 10,
    reviewedBy: null,
    machineResult: 'fail',
    machineConfidence: 0.42,
  }

  it('改判说明带上机器结论与置信度', () => {
    const note = overrideNote({ machineResult: 'fail', machineConfidence: 0.42, humanReview: review })
    expect(note).toBe('机器判不合格(0.42) → 人工改判合格')
  })

  it('无改判不编造说明', () => {
    expect(overrideNote({ machineResult: 'fail', machineConfidence: 0.42, humanReview: null })).toBe('')
    expect(overridden({ machineResult: 'fail', humanReview: null })).toBe(false)
  })

  it('人工结论与机器一致 = 不构成改判覆盖', () => {
    expect(overridden({ machineResult: 'fail', humanReview: { ...review, humanStatus: 'fail' } })).toBe(false)
    expect(overridden({ machineResult: 'fail', humanReview: review })).toBe(true)
  })
})

describe('排序：最没把握的先看（null 最前），同键稳定', () => {
  const rows = [
    row({ assetId: 3, fileName: 'c.jpg', absPath: '/b/c.jpg', machineConfidence: 0.9, ruleId: 'F2' }),
    row({ assetId: 1, fileName: 'a.jpg', absPath: '/a/a.jpg', machineConfidence: null, ruleId: 'F1' }),
    row({ assetId: 2, fileName: 'b.jpg', absPath: '/a/b.jpg', machineConfidence: 0.42, ruleId: 'F1' }),
    row({ assetId: 3, fileName: 'c.jpg', absPath: '/b/c.jpg', machineConfidence: 0.42, ruleId: 'F1' }),
  ]

  it('confidence：0.42 < 0.9，null 视为最不可知排最前', () => {
    const ids = sortRows(rows, 'confidence').map((r) => `${r.assetId}/${r.ruleId}`)
    expect(ids).toEqual(['1/F1', '2/F1', '3/F1', '3/F2'])
  })

  it('fileName / path / asset 三种序都真的换序', () => {
    expect(sortRows(rows, 'fileName').map((r) => r.fileName)).toEqual(['a.jpg', 'b.jpg', 'c.jpg', 'c.jpg'])
    expect(sortRows(rows, 'path').map((r) => r.absPath)).toEqual([
      '/a/a.jpg',
      '/a/b.jpg',
      '/b/c.jpg',
      '/b/c.jpg',
    ])
    expect(sortRows(rows, 'asset').map((r) => r.assetId)).toEqual([1, 2, 3, 3])
  })

  it('不改原数组（React state 不能被就地排序）', () => {
    const before = rows.map((r) => r.assetId)
    sortRows(rows, 'confidence')
    expect(rows.map((r) => r.assetId)).toEqual(before)
  })
})

describe('两级聚合（队列视图）：资产 → 规则', () => {
  const rows = [
    row({ assetId: 7, ruleId: 'F1', machineResult: 'undetermined', finalStatus: 'undetermined' }),
    row({ assetId: 7, ruleId: 'F2', machineResult: 'fail', finalStatus: 'fail' }),
    row({ assetId: 9, ruleId: 'F1', machineResult: 'fail', finalStatus: 'pass', reviewed: true }),
    row({ assetId: 9, ruleId: 'F2', machineResult: 'pass', finalStatus: 'pass' }),
  ]

  it('按资产分组、组内保留顺序、行数守恒', () => {
    const g = groupByAsset(rows)
    expect(g.map((x) => x.assetId)).toEqual([7, 9])
    expect(g[0].rows.map((r) => r.ruleId)).toEqual(['F1', 'F2'])
    expect(rowCount(g)).toBe(rows.length)
  })

  it('组档位取最差（不合格 > 未判定 > 合格），组内任一改判则标记已改判', () => {
    const g = groupByAsset(rows)
    expect(g[0].worst).toBe('fail')
    expect(g[1].worst).toBe('pass')
    expect(g[1].reviewed).toBe(true)
  })

  it('worstTier 直接测：fail 压过 undetermined', () => {
    expect(worstTier(['undetermined', 'fail'])).toBe('fail')
    expect(worstTier(['undetermined', 'pass'])).toBe('undetermined')
    expect(worstTier(['pass', 'skipped'])).toBe('pass')
    expect(worstTier([])).toBe('pass')
  })
})
