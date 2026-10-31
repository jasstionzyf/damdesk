import { describe, expect, it } from 'vitest'

import {
  artifactBasename,
  artifactPath,
  cellText,
  cellValue,
  filterRows,
  normalizeColumns,
  sortRows,
  tableColumns,
  toCsv,
  type ResultRowView,
} from '../src/lib/resultColumns'

/**
 * N5b 列口径（前端侧）：闭集校验 / 默认列 / judge 追加审核列 / 排序筛选导出一份口径。
 * 与 Rust `pack_host::result_columns` 同闭集 —— 两边改一处这里就红。
 */

const rows: ResultRowView[] = [
  { assetId: 1, fileName: 'a.jpg', result: 'ok', verdict: { status: 'pass', confidence: 0.9 }, fields: { n: 3 } },
  { assetId: 2, fileName: 'b.jpg', result: 'failed', verdict: null, reason: 'decode_failed', fields: { n: 1 } },
  { assetId: 3, fileName: 'c.jpg', result: 'skipped', verdict: null, fields: {} },
]

describe('normalizeColumns', () => {
  it('丢非法项、按 order 排序、重复 key 只留第一条', () => {
    const { columns, warnings } = normalizeColumns([
      { key: 'b', label: 'B', type: 'number', order: 5 },
      { key: 'a', label: 'A', type: 'text', order: 1 },
      { key: 'a', label: 'A2', type: 'bool', order: 2 },
      { label: '没 key', type: 'text' },
      { key: 'x', label: 'X', type: 'madeup' },
    ])
    expect(columns!.map((c) => c.key)).toEqual(['a', 'b'])
    expect(columns![0].type).toBe('text')
    expect(warnings.length).toBe(3)
  })

  it('空/全非法 = 未声明（默认列），不报错', () => {
    for (const raw of [[], [{ key: 'x', type: 'wrong' }], null]) {
      const { columns } = normalizeColumns(raw)
      expect(columns).toBeNull()
    }
  })
})

describe('tableColumns', () => {
  it('未声明 → 默认列（资产/执行结果/产物/备注）', () => {
    expect(tableColumns(null, ['process']).map((c) => c.key)).toEqual([
      '__asset',
      '__result',
      '__artifacts',
      '__note',
    ])
  })

  it('judge 追加审核列，process 一定没有', () => {
    const judge = tableColumns(null, ['judge']).map((c) => c.key)
    expect(judge.slice(-3)).toEqual(['__verdict', '__reason', '__confidence'])
    const process = tableColumns(null, ['process']).map((c) => c.key)
    expect(process.some((k) => k.startsWith('__verdict'))).toBe(false)
    expect(process).not.toContain('__reason')
  })

  it('声明列插在固定列之后、审核列之前', () => {
    const cols = tableColumns([{ key: 'dev', label: '偏差', type: 'number' }], ['judge'])
    expect(cols.map((c) => c.key)).toEqual([
      '__asset',
      '__result',
      'dev',
      '__verdict',
      '__reason',
      '__confidence',
    ])
  })
})

describe('取值/排序/筛选/导出', () => {
  const cols = tableColumns([{ key: 'n', label: '数量', type: 'number' }], ['judge'])

  it('行业列从 fields 取，缺失 = —', () => {
    const nCol = cols.find((c) => c.key === 'n')!
    expect(cellText(nCol, rows[0])).toBe('3')
    expect(cellText(nCol, rows[2])).toBe('—')
  })

  it('数字列按值排序，空值殿后且不随方向翻转', () => {
    const nCol = cols.find((c) => c.key === 'n')!
    expect(sortRows(rows, nCol, 'desc').map((r) => r.assetId)).toEqual([1, 2, 3])
    expect(sortRows(rows, nCol, 'asc').map((r) => r.assetId)).toEqual([2, 1, 3])
  })

  it('逐列筛选（大小写不敏感）', () => {
    const resultCol = cols.find((c) => c.key === '__result')!
    const hit = filterRows(rows, [resultCol], { __result: '失败' })
    expect(hit.map((r) => r.assetId)).toEqual([2])
  })

  it('CSV：BOM + 表头用 label（数字列带单位）+ 值转义', () => {
    const csv = toCsv(
      [{ key: 'n', label: '数量', type: 'number', unit: '张', kind: 'industry' }],
      [{ assetId: 9, result: 'ok', fields: { n: 12 } }],
    )
    expect(csv.startsWith('\uFEFF')).toBe(true)
    expect(csv).toContain('数量(张)')
    expect(csv).toContain('12张')
  })
})

/**
 * PROC-RES-2：处理类恒有产物列 + 产物条目归一（对象/裸字符串两形态）。
 */
describe('PROC-RES-2：产物列口径', () => {
  const declared = [
    { key: 'out_size', label: '输出尺寸', type: 'text' as const },
    { key: 'ratio', label: '宽高比', type: 'number' as const },
  ]

  it('process：声明了行业列也追加产物列（产物是处理结果的本体）', () => {
    const cols = tableColumns(declared, ['process'])
    const keys = cols.map((c) => c.key)
    expect(keys).toContain('__artifacts')
    expect(keys.indexOf('__artifacts')).toBeGreaterThan(keys.indexOf('ratio'))
    // 不重复追加（默认列里已有产物列时）
    expect(keys.filter((k) => k === '__artifacts')).toHaveLength(1)
    const art = cols.find((c) => c.key === '__artifacts')!
    expect(art.kind).toBe('fixed')
  })

  it('judge：不追加产物列（判定类产物列语义只在默认列里）', () => {
    const cols = tableColumns(declared, ['judge'])
    expect(cols.map((c) => c.key)).not.toContain('__artifacts')
  })

  it('artifactPath/artifactBasename：对象与裸字符串同形态归一', () => {
    expect(artifactPath('/a/b.webp')).toBe('/a/b.webp')
    expect(artifactPath({ kind: 'export_file', path: '/a/b.webp' })).toBe('/a/b.webp')
    expect(artifactPath({ kind: 'export_file', path: '' })).toBeNull()
    expect(artifactBasename('/data/fixes/job-1/a.resize.webp')).toBe('a.resize.webp')
    expect(artifactBasename({ kind: 'export_file', path: 'C:\\x\\y.png' })).toBe('y.png')
    expect(artifactBasename('plain.webp')).toBe('plain.webp')
  })

  it('cellValue __artifacts：文件名列表（不糊目录路径）', () => {
    const col = { key: '__artifacts', label: '产物', type: 'text' as const, kind: 'fixed' as const }
    const row = {
      assetId: 1,
      result: 'ok' as const,
      artifacts: [
        { kind: 'export_file', path: '/fixes/job-1/a.webp' },
        '/fixes/job-1/b.webp',
      ],
    }
    expect(cellValue(col, row)).toBe('a.webp；b.webp')
    expect(cellValue(col, { assetId: 2, result: 'ok' as const })).toBe('—')
  })
})
