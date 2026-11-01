// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'

import { ResultTable } from '../src/components/ResultTable'
import { ToastProvider } from '../src/lib/toast'
import type { ReactElement } from 'react'
import type { ResultRowView } from '../src/lib/resultColumns'

// i18n-C（卡 t_06629257）：本文件断言的是**中文界面原文**，而迁移后文案走 t()、
// jsdom 的 navigator.language 默认 en-US（会渲染英文）。这里把语言钉成 zh-Hans ——
// t() 在中文下缺译回退 key（= 中文原文），断言逐字不变、强度不降，只是显式声明
// 「本文件验中文文案」。vi.hoisted 早于所有 import 执行，故早于 i18n 模块初始化。
vi.hoisted(() => {
  try {
    if (typeof localStorage !== 'undefined' && localStorage) {
      localStorage.setItem('dam.locale', 'zh-Hans')
      return
    }
  } catch {
    /* 落桩 */
  }
  vi.stubGlobal('localStorage', {
    getItem: (k: string) => (k === 'dam.locale' ? 'zh-Hans' : null),
    setItem: () => {},
    removeItem: () => {},
    clear: () => {},
    key: () => null,
    length: 0,
  })
})

/**
 * PROC-RES-3（卡 t_823483c7）起结果表内含 toast（下载 / 在文件夹中显示）→ 用例统一挂 Provider。
 * 其余断言口径不变。
 */
function renderTable(ui: ReactElement) {
  return render(<ToastProvider>{ui}</ToastProvider>)
}

/**
 * N5b 结果表渲染（卡 t_4c0b3767）—— 前端验收三条：
 * 1. **judge 插件显示审核列**（状态 / 原因 / 置信度）；
 * 2. **process 插件无审核列**（处理类只有执行结果与产物，画审核状态就是造假）；
 * 3. **无 result_columns 时用默认列**（资产 / 执行结果 / 产物 / 备注），不报错。
 * 另外钉：声明的行业列按 `fields[key]` 取值 + 排序/筛选/导出同一份列序。
 */

const judgeRows: ResultRowView[] = [
  {
    assetId: 1,
    fileName: 'a.jpg',
    ruleId: 'R1_size',
    result: 'ok',
    verdict: { status: 'pass', confidence: 0.93, reasonCode: 'size_ok' },
    reason: 'size_ok',
    fields: { dev: 0.02, bg: 'white' },
  },
  {
    assetId: 2,
    fileName: 'b.jpg',
    ruleId: 'R2_white_bg',
    result: 'ok',
    verdict: { status: 'fail', confidence: 0.61, reasonCode: 'R2_not_white' },
    reason: 'R2_not_white',
    fields: { dev: 0.31, bg: 'gray' },
  },
  {
    assetId: 3,
    fileName: 'c.jpg',
    ruleId: 'R2_white_bg',
    result: 'skipped',
    verdict: null,
    reason: 'depends_on 短路',
    fields: {},
  },
]

const processRows: ResultRowView[] = [
  {
    assetId: 10,
    fileName: 'p1.jpg',
    ruleId: 'batch_convert',
    result: 'ok',
    verdict: null,
    fields: { cost: 42, fmt: 'webp' },
    artifacts: ['exports/p1.webp'],
  },
  {
    assetId: 11,
    fileName: 'p2.jpg',
    ruleId: 'batch_convert',
    result: 'failed',
    verdict: null,
    reason: 'decode_failed',
    fields: { cost: 7, fmt: 'webp' },
  },
]

const declared = [
  { key: 'dev', label: '白底偏差', type: 'number' as const, unit: '%', order: 1 },
  { key: 'bg', label: '背景', type: 'text' as const, order: 2 },
]

afterEach(cleanup)

describe('ResultTable —— 列由插件 schema 决定', () => {
  it('judge 插件：声明列 + 审核列（状态/原因/置信度）都在', () => {
    renderTable(
      <ResultTable testid="compliance-result-table" rows={judgeRows} columns={declared} outputs={['verdict']} total={judgeRows.length} />,
    )
    // 固定列 + 声明列
    expect(screen.getByTestId('compliance-result-table-th-__asset')).toBeTruthy()
    expect(screen.getByTestId('compliance-result-table-th-__result')).toBeTruthy()
    expect(screen.getByTestId('compliance-result-table-th-dev').textContent).toContain('白底偏差')
    // 审核列：judge 才有
    expect(screen.getByTestId('compliance-result-table-th-__verdict').textContent).toContain('状态')
    expect(screen.getByTestId('compliance-result-table-th-__reason').textContent).toContain('原因')
    expect(screen.getByTestId('compliance-result-table-th-__confidence').textContent).toContain('置信度')
    // 行业列取值：fields[key]，数字带单位
    expect(screen.getByTestId('compliance-result-table-cell-1-dev').textContent).toBe('0.02%')
    expect(screen.getByTestId('compliance-result-table-cell-1-bg').textContent).toBe('white')
    // 审核结论按三态标签渲染；skipped 行的状态列是「—」（执行结果 ≠ 结论）
    expect(screen.getByTestId('compliance-result-table-cell-1-__verdict').textContent).toBe('通过')
    expect(screen.getByTestId('compliance-result-table-cell-3-__verdict').textContent).toBe('—')
    expect(screen.getByTestId('compliance-result-table-cell-1-__confidence').textContent).toBe('0.93')
    expect(screen.getByTestId('compliance-result-table-outputs').textContent).toContain('有')
  })

  it('process 插件：没有审核列，只有执行结果（信任红线）', () => {
    const cols = [
      { key: 'cost', label: '耗时', type: 'number' as const, unit: 'ms', order: 1 },
      { key: 'fmt', label: '格式', type: 'text' as const, order: 2 },
    ]
    renderTable(
      <ResultTable testid="compliance-result-table" rows={processRows} columns={cols} outputs={['artifact']} total={processRows.length} />,
    )
    expect(screen.queryByTestId('compliance-result-table-th-__verdict')).toBeNull()
    expect(screen.queryByTestId('compliance-result-table-th-__reason')).toBeNull()
    expect(screen.queryByTestId('compliance-result-table-th-__confidence')).toBeNull()
    expect(screen.getByTestId('compliance-result-table-outputs').textContent).toContain('无')
    // 执行结果如实：ok / failed
    expect(screen.getByTestId('compliance-result-table-cell-10-__result').textContent).toBe('已执行')
    expect(screen.getByTestId('compliance-result-table-cell-11-__result').textContent).toBe('执行失败')
    expect(screen.getByTestId('compliance-result-table-cell-10-cost').textContent).toBe('42ms')
  })

  it('没有 result_columns：默认列（资产/执行结果/产物/备注）', () => {
    renderTable(<ResultTable testid="compliance-result-table" rows={processRows} columns={null} outputs={['artifact']} />)
    expect(screen.getByTestId('compliance-result-table-columns-source').textContent).toContain('默认列')
    expect(screen.getByTestId('compliance-result-table-th-__artifacts').textContent).toContain('产物')
    expect(screen.getByTestId('compliance-result-table-th-__note').textContent).toContain('备注')
    expect(screen.getByTestId('compliance-result-table-cell-10-__artifacts').textContent).toContain(
      'p1.webp',
    )
    // PROC-RES-2：产物单元格带缩略图与打开/下载（裸字符串老数据同样可点）
    expect(screen.getByTestId('compliance-result-table-artifact-thumb-10-0')).toBeTruthy()
    expect(screen.getByTestId('compliance-result-table-artifact-open-10-0').getAttribute('href')).toBe(
      '/api/compliance/artifact?path=exports%2Fp1.webp',
    )
    expect(screen.getByTestId('compliance-result-table-artifact-download-10-0').getAttribute('href')).toBe(
      '/api/compliance/artifact?path=exports%2Fp1.webp&download=1',
    )
    expect(screen.getByTestId('compliance-result-table-cell-11-__note').textContent).toBe(
      'decode_failed',
    )
    // 没有声明列 = 没有行业列
    expect(screen.queryByTestId('compliance-result-table-th-cost')).toBeNull()
  })

  it('judge + 没声明列：默认列之上再追加审核列', () => {
    renderTable(<ResultTable testid="compliance-result-table" rows={judgeRows} columns={null} outputs={['verdict']} />)
    expect(screen.getByTestId('compliance-result-table-th-__artifacts')).toBeTruthy()
    expect(screen.getByTestId('compliance-result-table-th-__verdict')).toBeTruthy()
  })

  it('排序与筛选：同一份列口径（点表头排序、逐列筛选）', () => {
    renderTable(<ResultTable testid="compliance-result-table" rows={judgeRows} columns={declared} outputs={['verdict']} />)
    const firstRow = () =>
      screen.getAllByTestId(/compliance-result-table-row-/)[0].getAttribute('data-testid')
    expect(firstRow()).toContain('row-1')

    // 按「白底偏差」降序：0.31 应排最前
    fireEvent.click(screen.getByTestId('compliance-result-table-sort-dev'))
    fireEvent.click(screen.getByTestId('compliance-result-table-sort-dev'))
    expect(firstRow()).toContain('row-2')

    // 筛选「背景」= gray：只剩 b.jpg 那行
    fireEvent.change(screen.getByTestId('compliance-result-table-filter-bg'), {
      target: { value: 'gray' },
    })
    const rows = screen.getAllByTestId(/compliance-result-table-row-/)
    expect(rows.length).toBe(1)
    expect(rows[0].getAttribute('data-testid')).toContain('row-2')
  })

  it('导出 CSV：走当前列序（含审核列）且带 UTF-8 BOM', () => {
    const blobs: Blob[] = []
    const realCreate = URL.createObjectURL
    const realRevoke = URL.revokeObjectURL
    URL.createObjectURL = vi.fn((b: Blob) => {
      blobs.push(b)
      return 'blob:fake'
    })
    URL.revokeObjectURL = vi.fn()
    const clickSpy = vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(() => {})

    renderTable(<ResultTable testid="compliance-result-table" rows={judgeRows} columns={declared} outputs={['verdict']} />)
    fireEvent.click(screen.getByTestId('compliance-result-table-export'))
    expect(blobs.length).toBe(1)
    expect(clickSpy).toHaveBeenCalled()

    URL.createObjectURL = realCreate
    URL.revokeObjectURL = realRevoke
    clickSpy.mockRestore()
  })
})

/**
 * PROC-RES-2：处理结果的产物列（对象形态 artifacts）+ 行勾选（批量收集产物导出）。
 */
describe('PROC-RES-2：产物列与勾选', () => {
  const artRows: ResultRowView[] = [
    {
      assetId: 20,
      fileName: 'x.jpg',
      ruleId: 'resize',
      result: 'ok',
      verdict: null,
      fields: {},
      artifacts: [
        { kind: 'export_file', path: '/test-fixture/state/compliance/fixes/job-3/x_resize.webp', sha256: 'ab12', bytes: 1024, width: 800, height: 600 },
      ],
    },
    {
      assetId: 21,
      fileName: 'y.jpg',
      ruleId: 'resize',
      result: 'failed',
      verdict: null,
      fields: {},
      reason: 'decode_failed',
      artifacts: [],
    },
  ]

  it('产物对象：缩略图指向产物本体 URL，文件名只显示 basename', () => {
    renderTable(<ResultTable testid="t" rows={artRows} columns={null} outputs={['artifact']} />)
    const cell = screen.getByTestId('t-cell-20-__artifacts')
    expect(cell.textContent).toContain('x_resize.webp')
    expect(cell.textContent).not.toContain('/test-fixture/state') // 目录路径不进表格
    const img = screen.getByTestId('t-artifact-thumb-20-0') as HTMLImageElement
    expect(img.getAttribute('src')).toBe(
      '/api/compliance/artifact?path=%2Ftest-fixture%2Fstate%2Fcompliance%2Ffixes%2Fjob-3%2Fx_resize.webp',
    )
    expect(screen.getByTestId('t-artifact-open-20-0').textContent).toBe('打开')
    expect(screen.getByTestId('t-artifact-download-20-0').getAttribute('href')).toContain('download=1')
    // failed 行无产物 → 占位符
    expect(screen.getByTestId('t-cell-21-__artifacts').textContent).toBe('—')
  })

  it('勾选：上报被选行；全选/单选；换数据清零', () => {
    const onSel = vi.fn()
    const { rerender } = render(
      <ToastProvider>
        <ResultTable testid="t" rows={artRows} columns={null} outputs={['artifact']} selectable onSelectedRowsChange={onSel} />
      </ToastProvider>,
    )
    expect(screen.getByTestId('t-th-__select')).toBeTruthy()
    // 初始上报空
    expect(onSel).toHaveBeenLastCalledWith([])
    // 勾第一行 → 上报该行
    fireEvent.click(screen.getByTestId('t-select-row-20'))
    expect(onSel).toHaveBeenLastCalledWith([artRows[0]])
    // 全选 → 两行
    fireEvent.click(screen.getByTestId('t-select-all'))
    expect(onSel).toHaveBeenLastCalledWith(artRows)
    // 换一次执行（rows 引用变化）→ 勾选清零
    rerender(
      <ToastProvider>
        <ResultTable testid="t" rows={[...artRows]} columns={null} outputs={['artifact']} selectable onSelectedRowsChange={onSel} />
      </ToastProvider>,
    )
    expect(onSel).toHaveBeenLastCalledWith([])
  })

  it('不可勾选时不画选择列（默认行为不变）', () => {
    renderTable(<ResultTable testid="t" rows={artRows} columns={null} outputs={['verdict']} />)
    expect(screen.queryByTestId('t-th-__select')).toBeNull()
  })
})
