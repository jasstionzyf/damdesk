// @vitest-environment jsdom
/**
 * DM-B2（KI-37）L2：**「这一次的结果」**（`components/JobResults.tsx` → `JobResultView`）。
 *
 * 这张卡把原来「列表 + 明细」挤在一屏的表面拆成三级下钻：列表归 L1（`Tasks` 页），
 * 这个文件只守 L2 的口径（原 N12 / PROC-RES-2 的信任红线一条不松）：
 *  1. 表格行数 == 接口行数（默认不筛；筛了要在计数里明说「已筛出 N」）；
 *  2. 列来自服务端声明（`result_columns`），前端不认识也照画；
 *  3. 审核列 / 复核动作只有检查类有；处理类只有「执行结果 + 产物」，且产物可勾选导出；
 *  4. 判定分布卡的数字就是接口给的三态（通过 / 未通过 / 待人工看），点一下即过滤；
 *  5. 行首有缩略图（判的是图就得看见图），点图开 L3。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { MemoryRouter } from 'react-router-dom'

import { JobResultView } from '../src/components/JobResults'
import { ToastProvider } from '../src/lib/toast'
import { complianceJobToRow } from '../src/lib/jobs'
import { ApiError, type ComplianceJobView, type ComplianceResultsView } from '../src/lib/api'

// i18n-C（卡 task）：本文件断言的是**中文界面原文**，而迁移后文案走 t()、
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

const apiStub = vi.hoisted(() => ({
  complianceResults: vi.fn(),
  // KI-40：结果面还会取任务行明细（失败原因），明细是**附加信息**，这里给空明细
  complianceJob: vi.fn().mockResolvedValue({ tasks: [], run: { error: null } }),
  complianceReviewCreate: vi.fn(),
  appInfo: vi.fn().mockResolvedValue({ dataDir: '/data/state' }),
  complianceArtifactsExport: vi.fn().mockResolvedValue({ ok: true, total: 0, copied: 0, files: [], missing: [], dest: '' }),
  // DECouple-FE：按这次 job 的结果导出子集（DECouple-BE 新端点）
  complianceJobExport: vi.fn().mockResolvedValue({ ok: true, copied: 1, dest: '/data/state/exports/run-7-x', missing: [] }),
  thumbUrl: (id: number, size = 256) => `/api/media/thumb/${id}?size=${size}`,
  // PROC-RES-3：结果表的「在文件夹中显示」读能力开关（这里给空 = 未加载，按可用处理）
  capabilities: vi.fn().mockResolvedValue(null),
}))
vi.mock('../src/lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../src/lib/api')>()
  return {
    // 保留真实导出（ApiError 等类不能在桩里丢：`instanceof` 会炸）
    ...actual,
    api: apiStub,
    formatSize: (n: number) => `${n} B`,
    artifactUrl: (path: string, download = false) =>
      `/api/compliance/artifact?path=${encodeURIComponent(path)}${download ? '&download=1' : ''}`,
  }
})

function judgeJob(): ComplianceJobView {
  return {
    runId: 7,
    status: 'done',
    statusLabel: '已完成',
    startedAt: 1_760_000_000,
    finishedAt: 1_760_000_060,
    total: 2,
    done: 2,
    failed: 0,
    skipped: 0,
    cancelled: 0,
    queued: 0,
    durationMs: 60_000,
    error: null,
    params: { spec: { name: '我的主图' } },
    totalAssets: 2,
    verdictCounts: { pass: 1, fail: 1, undetermined: 0 },
  }
}

function processJob(): ComplianceJobView {
  return {
    ...judgeJob(),
    runId: 9,
    params: { spec: { name: '缩到 800', actions: [{ action: 'resize', params: { resizeLongEdge: 800 } }] } },
    verdictCounts: null,
  }
}

function resultsView(kinds: ('judge' | 'process')[], columns: ComplianceResultsView['columns']): ComplianceResultsView {
  return {
    packId: kinds.includes('judge') ? 'com.example.detect' : 'com.example.imaging',
    kinds,
    jobId: 7,
    runId: 7,
    total: 2,
    limit: 500,
    offset: 0,
    columns,
    columnWarnings: [],
    items: [
      {
        assetId: 11,
        fileName: 'a.jpg',
        absPath: '/lib/a.jpg',
        ruleId: kinds.includes('judge') ? 'R_size' : 'resize',
        judgementScope: null,
        result: 'ok',
        verdict: kinds.includes('judge') ? { status: 'pass', confidence: 0.93, reasonCode: null } : null,
        reason: null,
        fields: { out_size: '800x600', ratio: 1.33 },
        ts: 1_760_000_000,
        artifacts: kinds.includes('judge') ? null : [{ kind: 'export_file', path: '/data/state/compliance/fixes/job-9/b_resize.webp', sha256: 'ab12' }],
      },
      {
        assetId: 12,
        fileName: 'b.jpg',
        absPath: '/lib/b.jpg',
        ruleId: kinds.includes('judge') ? 'R_size' : 'resize',
        judgementScope: null,
        result: kinds.includes('judge') ? 'ok' : 'failed',
        verdict: kinds.includes('judge') ? { status: 'fail', confidence: 0.7, reasonCode: 'too_small' } : null,
        reason: '长边 600 < 800',
        fields: { out_size: '600x400', ratio: 1.5 },
        ts: 1_760_000_001,
        artifacts: null,
      },
    ],
  }
}

function renderL2(job: ComplianceJobView) {
  const onClose = vi.fn()
  const onChanged = vi.fn()
  // PROC-RES-3 起结果表内含 toast（产物下载/在文件夹中显示）→ 挂 Provider
  render(
    <ToastProvider>
      <MemoryRouter initialEntries={['/tasks']}>
        <JobResultView job={complianceJobToRow(job)} onClose={onClose} onChanged={onChanged} />
      </MemoryRouter>
    </ToastProvider>,
  )
  return { onClose, onChanged }
}

beforeEach(() => {
  apiStub.complianceResults.mockResolvedValue(resultsView(['judge'], null))
  apiStub.complianceReviewCreate.mockResolvedValue({ ok: true })
})

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
})

describe('L2 检查类：判定分布 + 过滤 + 表格 + 图入口', () => {
  it('分布卡数字 == 接口三态；表格行数 == 接口行数（默认不筛）', async () => {
    renderL2(judgeJob())
    const table = await screen.findByTestId('job-result-table')
    await waitFor(() => expect(screen.getByTestId('job-verdict-filter')).toBeTruthy())
    expect(screen.getByTestId('job-verdict-pass').getAttribute('data-count')).toBe('1')
    expect(screen.getByTestId('job-verdict-fail').getAttribute('data-count')).toBe('1')
    expect(screen.getByTestId('job-verdict-undetermined').getAttribute('data-count')).toBe('0')
    // 两行（一张图一行规则）——与接口 items 一致，没被前端悄悄吃掉
    expect(table.querySelectorAll('tbody tr').length).toBe(2)
    expect(screen.getByTestId('job-results-shown').textContent).toContain('显示 2 / 2 行')
  })

  it('点「未通过」→ 只剩那几张；关掉筛选回全部（计数如实说已筛出）', async () => {
    renderL2(judgeJob())
    const table = await screen.findByTestId('job-result-table')
    await waitFor(() => expect(table.querySelectorAll('tbody tr').length).toBe(2))
    fireEvent.click(screen.getByTestId('job-verdict-filter-fail'))
    await waitFor(() => expect(table.querySelectorAll('tbody tr').length).toBe(1))
    expect(table.textContent).toContain('长边 600 < 800')
    expect(screen.getByTestId('job-results-shown').textContent).toContain('显示 1 / 2 行')
    fireEvent.click(screen.getByTestId('job-verdict-filter-all'))
    await waitFor(() => expect(table.querySelectorAll('tbody tr').length).toBe(2))
  })

  it('审核列与复核入口都在；行首有缩略图（点图开 L3）', async () => {
    renderL2(judgeJob())
    expect(await screen.findByTestId('job-results-review-link')).toBeTruthy()
    const table = await screen.findByTestId('job-result-table')
    await waitFor(() => expect(table.textContent).toContain('未通过'))
    expect(screen.getByTestId('job-result-table-th-__thumb')).toBeTruthy()
    expect(screen.getByTestId('job-result-table-thumb-11')).toBeTruthy()
    fireEvent.click(screen.getByTestId('job-result-table-thumb-11'))
    expect(await screen.findByTestId('job-images-dialog')).toBeTruthy()
    expect(screen.getByTestId('job-images-grid').textContent).toContain('a.jpg')
  })

  it('L3 就地改判：走人工改判接口 + 刷新结果', async () => {
    const { onChanged } = renderL2(judgeJob())
    fireEvent.click(await screen.findByTestId('job-images-open'))
    fireEvent.click(await screen.findByTestId('job-image-cell-12'))
    fireEvent.click(screen.getByTestId('job-image-review-pass'))
    fireEvent.change(screen.getByTestId('job-image-review-reason'), { target: { value: '人工确认可用' } })
    fireEvent.click(screen.getByTestId('job-image-review-submit'))
    await waitFor(() =>
      expect(apiStub.complianceReviewCreate).toHaveBeenCalledWith({
        assetId: 12,
        humanStatus: 'pass',
        reason: '人工确认可用',
      }),
    )
    await waitFor(() => expect(onChanged).toHaveBeenCalled())
    expect(await screen.findByTestId('job-image-review-result')).toBeTruthy()
  })

  it('按这次结果导出：档位 + 目录原样提交给 DECouple-BE 端点，成功后如实报张数', async () => {
    renderL2(judgeJob())
    const bar = await screen.findByTestId('job-results-export-bar')
    await waitFor(() =>
      expect(within(bar).getByTestId('job-results-export-dest').getAttribute('value')).toContain(
        '/data/state/exports/run-7-',
      ),
    )
    fireEvent.change(within(bar).getByTestId('job-results-export-verdict'), { target: { value: 'fail' } })
    fireEvent.change(within(bar).getByTestId('job-results-export-dest'), { target: { value: '/tmp/judge-out' } })
    // 「导出（1 张）」：所选档位的张数取自这次执行的分布，点之前就知道拿多少
    expect(within(bar).getByTestId('job-results-export-by-result').textContent).toContain('1')
    fireEvent.click(within(bar).getByTestId('job-results-export-by-result'))
    await waitFor(() =>
      expect(apiStub.complianceJobExport).toHaveBeenCalledWith(7, {
        verdict: 'fail',
        destDir: '/tmp/judge-out',
      }),
    )
    await waitFor(() =>
      expect(screen.getByTestId('job-results-export-result').textContent).toContain('已导出 1 张'),
    )
  })

  it('端点还没上（404）→ 明说没导出任何文件，不谎报成功', async () => {
    apiStub.complianceJobExport.mockRejectedValueOnce(new ApiError('not found', 404, 'not_found'))
    renderL2(judgeJob())
    const bar = await screen.findByTestId('job-results-export-bar')
    fireEvent.click(within(bar).getByTestId('job-results-export-by-result'))
    await waitFor(() =>
      expect(screen.getByTestId('job-results-export-result').textContent).toContain('没有导出任何文件'),
    )
  })
})

describe('L2 处理类：只有执行结果与产物（不冒充审核）', () => {
  it('没有审核列、没有复核入口；产物可勾选导出', async () => {
    apiStub.complianceResults.mockResolvedValue(resultsView(['process'], null))
    renderL2(processJob())
    expect(await screen.findByTestId('job-results-no-audit')).toBeTruthy()
    expect(screen.queryByTestId('job-results-review-link')).toBeNull()
    expect(screen.queryByTestId('job-verdict-card')).toBeNull()
    const table = await screen.findByTestId('job-result-table')
    await waitFor(() => expect(table.textContent).toContain('长边 600 < 800'))
    expect(table.textContent).not.toContain('未通过')
    expect(table.textContent).not.toContain('待人工看')

    const exportBtn = screen.getByTestId('job-results-export-selected')
    expect(exportBtn.textContent).toContain('0')
    fireEvent.click(screen.getByTestId('job-result-table-select-row-11'))
    await waitFor(() => expect(exportBtn.textContent).toContain('1'))
    await waitFor(() =>
      expect(screen.getByTestId('job-results-export-dest').getAttribute('value')).toContain(
        '/data/state/exports/artifacts-',
      ),
    )
    fireEvent.change(screen.getByTestId('job-results-export-dest'), { target: { value: '/tmp/proc-out' } })
    fireEvent.click(exportBtn)
    await waitFor(() =>
      expect(apiStub.complianceArtifactsExport).toHaveBeenCalledWith({
        items: [{ path: '/data/state/compliance/fixes/job-9/b_resize.webp', name: 'b_resize.webp' }],
        destDir: '/tmp/proc-out',
      }),
    )
  })

  it('列由能力声明：声明了「输出尺寸 / 宽高比」就照画', async () => {
    apiStub.complianceResults.mockResolvedValue(
      resultsView(['process'], [
        { key: 'out_size', label: '输出尺寸', type: 'text' },
        { key: 'ratio', label: '宽高比', type: 'number' },
      ]),
    )
    renderL2(processJob())
    const table = await screen.findByTestId('job-result-table')
    await waitFor(() => expect(table.textContent).toContain('输出尺寸'))
    expect(table.textContent).toContain('宽高比')
    expect(table.textContent).toContain('800x600')
  })

  it('检查结果：不画产物选择列；导出条是「按这次结果导出子集」而不是产物勾选导出', async () => {
    renderL2(judgeJob())
    await screen.findByTestId('job-result-table')
    expect(screen.queryByTestId('job-result-table-th-__select')).toBeNull()
    // DECouple-FE（卡 task）：检查类的导出入口按「这次执行的结果」切片（全部/通过/未通过/待人工看），
    // 不是处理类那种「勾选产物行再导出」—— 勾选列必须仍然不存在。
    const bar = await screen.findByTestId('job-results-export-bar')
    expect(within(bar).getByTestId('job-results-export-verdict')).toBeTruthy()
    expect(within(bar).queryByTestId('job-results-export-selected')).toBeNull()
  })

  it('「收起」回调给 L1（回列表不丢筛选/滚动）', async () => {
    const { onClose } = renderL2(judgeJob())
    fireEvent.click(await screen.findByTestId('job-results-close'))
    expect(onClose).toHaveBeenCalled()
  })
})

/**
 * KI40-FIX：失败原因块（`job-failure-reasons`）。
 *
 * 真机实测的两处病：① 插件给的结构化失败（`target_over_limit`）被宿主兜底成
 * 「未返回产物」，用户拿不到可照做的信息；② 任务行当年是 `done`，块按
 * `status==='failed'` 过滤成空。这里守的口径：
 *  - 有失败结果行就渲染（旧数据的任务行还是 done 也一样）；
 *  - 文案是结果行 `reason` 的原文（与结果表同源，不做第二套兜底话术）；
 *  - 同一资产在任务行与结果行各出现一次时只算一条。
 */
describe('KI40-FIX：失败原因可见（任务行 ∪ 结果行，同一条只算一次）', () => {
  const PLUGIN_REASON =
    '目标 36264×13040（473,102,880 像素）超产物上限：像素 ≤1e8、长边 ≤2e4；' +
    '该源图可用上限 scale≤1.99；请调小 scale 或先缩小源图（失败动作 `upscale`）'

  function failedView(): ComplianceResultsView {
    return {
      packId: 'com.soujpg.pack.upscale',
      kinds: ['process'],
      jobId: 80,
      runId: 80,
      total: 1,
      limit: 500,
      offset: 0,
      columns: null,
      columnWarnings: [],
      items: [
        {
          assetId: 643,
          fileName: 'mi_multi_01.jpg',
          absPath: '/lib/mi_multi_01.jpg',
          ruleId: 'upscale',
          judgementScope: 'per_asset',
          result: 'failed',
          verdict: null,
          reason: PLUGIN_REASON,
          fields: { chain: [{ action: 'upscale', ms: 574, result: 'failed' }] },
          ts: 1_792_927_527,
          artifacts: null,
        },
      ],
    }
  }

  function taskRow(assetId: number, status: string, lastError: string | null) {
    return {
      jobId: 1,
      targetId: assetId,
      step: '超分 4×',
      assetId,
      status,
      statusLabel: status === 'failed' ? '执行失败' : '已完成',
      attempts: 1,
      lastError,
      updatedAt: 1_792_927_527,
    }
  }

  it('任务行 done（旧数据）+ 结果行 failed → 原因块渲染插件原文，不是兜底话术', async () => {
    apiStub.complianceResults.mockResolvedValue(failedView())
    apiStub.complianceJob.mockResolvedValue({
      tasks: [taskRow(643, 'done', null)],
      run: { error: null },
    })
    renderL2(processJob())
    const block = await screen.findByTestId('job-failure-reasons')
    expect(block.textContent).toContain('失败 1 条')
    expect(block.textContent).toContain('请调小 scale')
    expect(block.textContent).not.toContain('未返回产物')
  })

  it('任务行 failed + 同一资产的 lastError → 只算一条（不因两处来源重复计数）', async () => {
    apiStub.complianceResults.mockResolvedValue(failedView())
    apiStub.complianceJob.mockResolvedValue({
      tasks: [taskRow(643, 'failed', PLUGIN_REASON)],
      run: { error: null },
    })
    renderL2(processJob())
    const block = await screen.findByTestId('job-failure-reasons')
    expect(block.textContent).toContain('失败 1 条')
    expect(block.querySelectorAll('li').length).toBe(1)
  })

  it('只有任务行失败（失败到没有结果行）也要给原因', async () => {
    apiStub.complianceResults.mockResolvedValue({ ...failedView(), total: 0, items: [] })
    apiStub.complianceJob.mockResolvedValue({
      tasks: [taskRow(12, 'failed', '宿主网关调用失败：io 读不出 /lib/b.jpg（失败动作 `upscale`）')],
      run: { error: null },
    })
    renderL2(processJob())
    const block = await screen.findByTestId('job-failure-reasons')
    expect(block.textContent).toContain('io 读不出 /lib/b.jpg')
  })
})
