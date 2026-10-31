// @vitest-environment jsdom
/**
 * DM-B2（KI-37）：**三级下钻**的端到端 DOM 存证（L1 统一列表 → L2 这次的结果 → L3 图列表 Dialog）。
 *
 * 卡上验收（headless 走查在容器里另跑，这里守组件行为与文案）：
 *  1. L1 六类都出现（检查 / 处理 / 导入 / 导出 / 打标 / 去重），**运行中置顶**；
 *  2. 点进 L2：分布卡数字与接口一致，表格行数 == 接口行数；
 *  3. L3：开 Dialog → 点单图 → 关掉回 L2，**过滤与选中都不丢**；
 *  4. 文案：不再有「只保留最近一次导出」这类误伤检查结果的表述；用户面不出现 job/verdict 等内部词。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter } from 'react-router-dom'

import type { ComplianceJobView, ComplianceResultsView, ImportBatchRecord } from '../src/lib/api'
import type { Task } from '../src/lib/tasks'

const apiStub = vi.hoisted(() => ({
  complianceJobs: vi.fn(),
  complianceResults: vi.fn(),
  // KI-40：结果面还会取任务行明细（失败原因），明细是**附加信息**，这里给空明细
  complianceJob: vi.fn().mockResolvedValue({ tasks: [], run: { error: null } }),
  complianceReviewCreate: vi.fn(),
  complianceArtifactsExport: vi.fn().mockResolvedValue({ ok: true, total: 0, copied: 0, files: [], missing: [], dest: '' }),
  importBatches: vi.fn(),
  exportStatus: vi.fn(),
  exportCancel: vi.fn(),
  appInfo: vi.fn().mockResolvedValue({ dataDir: '/data/state' }),
  capabilities: vi.fn().mockResolvedValue({
    isDesktop: false,
    storeMode: 'server-volume',
    writableRoots: [],
    roots: [],
    features: {},
    featureReasons: {},
  }),
  thumbUrl: (id: number, size = 256) => `/api/media/thumb/${id}?size=${size}`,
}))

vi.mock('../src/lib/api', () => ({
  api: apiStub,
  formatSize: (n: number) => `${n} B`,
  artifactUrl: (path: string) => `/api/compliance/artifact?path=${encodeURIComponent(path)}`,
}))

const taskStub = vi.hoisted(() => ({ tasks: [] as Task[] }))
vi.mock('../src/lib/tasks', () => ({
  useTasks: () => ({
    tasks: taskStub.tasks,
    runningCount: taskStub.tasks.filter((t) => t.status === 'running').length,
    startImport: vi.fn(),
    startEmbed: vi.fn(),
    startTag: vi.fn(),
    cancel: vi.fn(),
    retry: vi.fn(),
    retryFailed: vi.fn(),
    dismiss: vi.fn(),
  }),
  TASK_KIND_LABEL: { import: '导入', embed: '嵌入', tag: '打标', dedup: '去重', export: '导出' },
}))

vi.mock('../src/lib/toast', () => ({
  useToast: () => ({ toast: vi.fn(), dismiss: vi.fn() }),
  ToastProvider: ({ children }: { children: unknown }) => children,
}))

import { Tasks } from '../src/pages/Tasks'

// i18n-C（卡 task）：本文件断言的是**中文界面原文**，而迁移后文案走 t()、
// jsdom 的 navigator.language 默认 en-US（会渲染英文）。这里把语言钉成 zh-Hans ——
// t() 在中文下缺译回退 key（= 中文原文），断言逐字不变、强度不降，只是显式声明
// 「本文件验中文文案」。vi.hoisted 早于所有 import 执行，故早于 i18n 模块初始化。
vi.hoisted(() => {
  // jsdom 文件有真实 localStorage；node 环境的纯 SSR 渲染测试没有它 ——
  // 给一个最小桩，目的只有一个：让 i18n 初始化时 resolveInitialLocale() 读到 zh-Hans。
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

function run(over: Partial<ComplianceJobView> & { runId: number }): ComplianceJobView {
  return {
    status: 'done',
    statusLabel: '已完成',
    startedAt: 1_760_000_000 + over.runId,
    finishedAt: 1_760_000_060 + over.runId,
    total: 2,
    done: 2,
    failed: 0,
    skipped: 0,
    cancelled: 0,
    queued: 0,
    durationMs: 60_000,
    error: null,
    params: { spec: { name: `方案${over.runId}` } },
    totalAssets: 2,
    verdictCounts: { pass: 1, fail: 1, undetermined: 0 },
    ...over,
  }
}

const JUDGE = run({ runId: 7, params: { spec: { name: '线上主图' } } })
const PROCESS = run({
  runId: 9,
  params: { spec: { name: '缩到 800', actions: [{ action: 'resize', params: {} }] } },
  totalAssets: 1,
  verdictCounts: null,
})
const RUNNING = run({
  runId: 20,
  status: 'running',
  statusLabel: '进行中',
  finishedAt: null,
  total: 5,
  done: 2,
  totalAssets: null,
  verdictCounts: null,
})

function judgeResults(): ComplianceResultsView {
  return {
    packId: 'com.example.detect',
    kinds: ['judge'],
    jobId: 7,
    runId: 7,
    total: 2,
    limit: 500,
    offset: 0,
    columns: null,
    columnWarnings: [],
    items: [
      {
        assetId: 11,
        fileName: 'a.jpg',
        absPath: '/lib/a.jpg',
        ruleId: 'R_size',
        judgementScope: null,
        result: 'ok',
        verdict: { status: 'pass', confidence: 0.93, reasonCode: null },
        reason: null,
        fields: {},
        ts: 1_760_000_000,
        artifacts: null,
      },
      {
        assetId: 12,
        fileName: 'b.jpg',
        absPath: '/lib/b.jpg',
        ruleId: 'R_size',
        judgementScope: null,
        result: 'ok',
        verdict: { status: 'fail', confidence: 0.7, reasonCode: 'too_small' },
        reason: '长边 600 < 800',
        fields: {},
        ts: 1_760_000_001,
        artifacts: null,
      },
    ],
  }
}

const BATCH: ImportBatchRecord = {
  batchId: 41,
  rootPath: '/corpus/incoming',
  storeMode: 'copyin',
  status: 'done',
  totalFiles: 120,
  doneFiles: 120,
  error: null,
  startedAt: 1_750_000_000,
  endedAt: 1_750_000_060,
}

function renderTasks(url = '/tasks') {
  return render(
    <MemoryRouter initialEntries={[url]}>
      <Tasks />
    </MemoryRouter>,
  )
}

beforeEach(() => {
  apiStub.complianceJobs.mockResolvedValue({ jobs: [RUNNING, JUDGE, PROCESS], total: 3 })
  apiStub.complianceResults.mockResolvedValue(judgeResults())
  apiStub.complianceReviewCreate.mockResolvedValue({ ok: true })
  apiStub.importBatches.mockResolvedValue([BATCH])
  apiStub.exportStatus.mockResolvedValue({
    running: false,
    progress: null,
    report: {
      total: 20,
      exported: 12,
      skipped: [{ assetId: 7, fileName: 'a.jpg', reason: '文件已失联' }],
      errors: [],
      cancelled: false,
      dest: '/corpus/exports/export-1',
      manifestWritten: true,
    },
    error: null,
  })
  taskStub.tasks = [
    {
      id: 3,
      kind: 'tag',
      title: '主图打标',
      detail: '模板：电商主图',
      current: 10,
      total: 10,
      status: 'done',
      startedAt: 1_759_000_000_000,
    },
    {
      id: 4,
      kind: 'dedup',
      title: '查重扫描',
      detail: '全库',
      current: 40,
      total: 40,
      status: 'done',
      startedAt: 1_759_500_000_000,
    },
  ]
})

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
})

describe('L1 统一列表', () => {
  it('六类都在一个列表里，运行中置顶，行里能读出时间·类型·张数·结果', async () => {
    renderTasks()
    const list = await screen.findByTestId('job-list')
    // 行 = 统一 job（不是类型数）：运行中的检查 + 两次检查/处理 + 导入批次 + 导出 + 打标 + 去重
    const rows = list.querySelectorAll('[data-job-key]')
    expect(rows.length).toBe(7)
    const types = [...list.querySelectorAll('[data-job-key]')].map((r) => r.getAttribute('data-job-type'))
    expect(types).toEqual(expect.arrayContaining(['judge', 'process', 'import', 'export', 'tag', 'dedup']))
    // 运行中置顶
    expect(rows[0].getAttribute('data-status')).toBe('running')
    expect(rows[0].getAttribute('data-job-type')).toBe('judge')
    // 行摘要：检查类给三态分布，处理类给成功/失败/跳过
    expect(screen.getByTestId('job-row-summary-compliance:7').textContent).toContain('通过 1')
    expect(screen.getByTestId('job-row-summary-compliance:7').textContent).toContain('未通过 1')
    expect(screen.getByTestId('job-row-summary-compliance:9').textContent).toContain('成功 2')
    // 张数（本次判了多少张）
    expect(screen.getByTestId('job-row-count-compliance:7').textContent).toContain('2 张')
  })

  it('类型筛选：检查 / 处理 / 导入导出 / 打标 各只剩自己的行', async () => {
    renderTasks()
    await screen.findByTestId('job-list')
    fireEvent.click(screen.getByTestId('job-list-filter-judge'))
    await waitFor(() =>
      expect(screen.getByTestId('job-list').querySelectorAll('[data-job-key]').length).toBe(2),
    )
    fireEvent.click(screen.getByTestId('job-list-filter-process'))
    await waitFor(() =>
      expect(screen.getByTestId('job-list').querySelectorAll('[data-job-key]').length).toBe(1),
    )
    fireEvent.click(screen.getByTestId('job-list-filter-io'))
    await waitFor(() =>
      expect(screen.getByTestId('job-list').querySelectorAll('[data-job-key]').length).toBe(2),
    )
    fireEvent.click(screen.getByTestId('job-list-filter-tag'))
    await waitFor(() =>
      expect(screen.getByTestId('job-list').querySelectorAll('[data-job-key]').length).toBe(1),
    )
  })

  it('空态说清下一步去哪（不给空白页）', async () => {
    apiStub.complianceJobs.mockResolvedValue({ jobs: [], total: 0 })
    apiStub.importBatches.mockResolvedValue([])
    apiStub.exportStatus.mockResolvedValue({ running: false, progress: null, report: null, error: null })
    taskStub.tasks = []
    renderTasks()
    const empty = await screen.findByTestId('job-list-empty')
    expect(empty.textContent).toContain('送去处理')
    expect(empty.textContent).not.toContain('判定')
  })
})

describe('L2 点一行看这一次的结果', () => {
  it('点检查行 → 就地展开；分布数字与接口一致；表格行数 == 接口行数', async () => {
    renderTasks()
    fireEvent.click(await screen.findByTestId('job-row-compliance:7'))
    await waitFor(() => expect(apiStub.complianceResults).toHaveBeenCalledWith({ runId: 7, limit: 500 }))
    const panel = await screen.findByTestId('job-detail-panel')
    expect(panel.querySelector('[data-testid="job-verdict-pass"]')?.getAttribute('data-count')).toBe('1')
    expect(panel.querySelector('[data-testid="job-verdict-fail"]')?.getAttribute('data-count')).toBe('1')
    const table = await screen.findByTestId('job-result-table')
    await waitFor(() => expect(table.querySelectorAll('tbody tr').length).toBe(2))
  })

  it('深链 ?job=7 直接展开那一次（刷新/分享可恢复）', async () => {
    renderTasks('/tasks?job=7')
    await waitFor(() => expect(apiStub.complianceResults).toHaveBeenCalledWith({ runId: 7, limit: 500 }))
    expect(await screen.findByTestId('job-detail-panel')).toBeTruthy()
  })

  it('导出 / 导入行各自展开自己的结果面（原分区能力不回退）', async () => {
    renderTasks()
    fireEvent.click(await screen.findByTestId('job-row-export:last'))
    expect(await screen.findByTestId('job-export-detail')).toBeTruthy()
    await waitFor(() =>
      expect(screen.getByTestId('export-last-summary').textContent).toContain('最近一次：导出 12/20 张'),
    )
    fireEvent.click(screen.getByTestId('job-row-import:41'))
    await waitFor(() => expect(screen.getByTestId('job-import-detail')).toBeTruthy())
    expect(screen.getByTestId('job-import-detail').textContent).toContain('复制入库')
  })
})

describe('L3 图列表 Dialog', () => {
  it('开 → 点单图 → 关掉回 L2：过滤与选中不丢', async () => {
    renderTasks()
    fireEvent.click(await screen.findByTestId('job-row-compliance:7'))
    await screen.findByTestId('job-result-table')
    // 先过滤到「未通过」再开图列表 —— L3 只该看到这一张
    fireEvent.click(screen.getByTestId('job-verdict-filter-fail'))
    await waitFor(() =>
      expect(screen.getByTestId('job-result-table').querySelectorAll('tbody tr').length).toBe(1),
    )
    fireEvent.click(screen.getByTestId('job-images-open'))
    const dialog = await screen.findByTestId('job-images-dialog')
    expect(dialog.textContent).toContain('b.jpg')
    expect(dialog.textContent).not.toContain('a.jpg')
    fireEvent.click(await screen.findByTestId('job-image-cell-12'))
    expect(await screen.findByTestId('job-image-detail')).toBeTruthy()
    // 关闭 Dialog（✕）→ 回 L2，筛选还在
    fireEvent.click(screen.getByTestId('job-images-dialog-close'))
    await waitFor(() => expect(screen.queryByTestId('job-images-dialog')).toBeNull())
    expect(screen.getByTestId('job-detail-panel').querySelector('[data-testid="job-result-table"]')).toBeTruthy()
    expect(screen.getByTestId('job-result-table').querySelectorAll('tbody tr').length).toBe(1)
  })
})

describe('文案（用户面不出现内部词、不再吓人）', () => {
  it('保留口径说「检查结果一直都在」，不出现「只保留最近一次导出」', async () => {
    renderTasks()
    const note = await screen.findByTestId('tasks-retention-note')
    expect(note.textContent).toContain('一直都在')
    expect(note.textContent).toContain('最近 50 次')
    expect(document.body.textContent ?? '').not.toContain('只保留最近一次导出')
  })

  it('用户可见文案不含 job / verdict / 插件 / 规格 / 行业包', async () => {
    renderTasks()
    await screen.findByTestId('job-list')
    fireEvent.click(screen.getByTestId('job-row-compliance:7'))
    await screen.findByTestId('job-result-table')
    const visible = document.body.textContent ?? ''
    for (const word of ['job', 'verdict', '插件', '规格', '行业包']) {
      expect(visible.toLowerCase()).not.toContain(word.toLowerCase())
    }
  })
})
