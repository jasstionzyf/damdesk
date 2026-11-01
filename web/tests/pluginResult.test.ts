import { describe, expect, it } from 'vitest'

import {
  ARTIFACT_KINDS,
  PLUGIN_KINDS,
  RESULT_LABELS,
  RESULT_STATUSES,
  VERDICT_LABELS,
  VERDICT_STATUSES,
  hasJudge,
  hasProcess,
  isJudgedRow,
  rowNote,
  type ResultRow,
} from '../src/lib/pluginResult'

/**
 * N5a 契约的**前端投影**：闭集 + outputs 判定 + 行级备注。
 * 渲染（result_columns / 默认列）是 N5b 的事，这里只钉语义。
 */
describe('pluginResult 结果行契约', () => {
  it('闭集三值/三态/outputs 与宿主一致', () => {
    expect([...RESULT_STATUSES]).toEqual(['ok', 'failed', 'skipped'])
    expect([...VERDICT_STATUSES]).toEqual(['pass', 'fail', 'undetermined'])
    expect([...PLUGIN_KINDS]).toEqual(['verdict', 'artifact'])
    expect([...ARTIFACT_KINDS]).toEqual(['working_version', 'export_file'])
    // `skipped` 只能在执行结果里，不许当审核状态
    expect((VERDICT_STATUSES as readonly string[]).includes('skipped')).toBe(false)
  })

  it('每个闭集值都有用户可读标签（自造状态会渲染成空白）', () => {
    for (const status of RESULT_STATUSES) expect(RESULT_LABELS[status]).toBeTruthy()
    for (const status of VERDICT_STATUSES) expect(VERDICT_LABELS[status]).toBeTruthy()
  })

  it('outputs 决定有没有审核面，不靠猜', () => {
    expect(hasJudge(['verdict'])).toBe(true)
    expect(hasJudge(['artifact'])).toBe(false)
    expect(hasJudge(undefined)).toBe(false)
    expect(hasProcess(['verdict', 'artifact'])).toBe(true)
    expect(hasProcess(['verdict'])).toBe(false)
  })

  it('处理类的行永远不算判定行（即使包里手写了 verdict）', () => {
    const row: ResultRow = {
      assetId: 'a1',
      result: 'ok',
      verdict: { status: 'pass', confidence: 1 },
    }
    expect(isJudgedRow(row, ['artifact'])).toBe(false)
    expect(isJudgedRow(row, ['verdict'])).toBe(true)
    // 判定类但这一行没有结论（执行失败）→ 也不是判定行
    expect(isJudgedRow({ assetId: 'a2', result: 'failed', error: { code: 'x', message: 'y' } }, ['verdict'])).toBe(
      false,
    )
  })

  it('行级备注三选一：判定原因 / 失败原因 / 跳过原因', () => {
    expect(rowNote({ assetId: 1, result: 'ok', verdict: { status: 'fail', reasonCode: 'A4_x' } })).toBe('A4_x')
    expect(rowNote({ assetId: 1, result: 'ok', verdict: { status: 'pass' } })).toBe('通过')
    expect(rowNote({ assetId: 1, result: 'failed', error: { code: 'asset_unreadable', message: '解码失败' } })).toBe(
      'asset_unreadable：解码失败',
    )
    expect(rowNote({ assetId: 1, result: 'skipped', skipReason: '原图缺失' })).toBe('原图缺失')
    expect(rowNote({ assetId: 1, result: 'ok' })).toBe('')
  })
})
