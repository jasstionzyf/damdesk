// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, waitFor } from '@testing-library/react'
import { MemoryRouter } from 'react-router-dom'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

/**
 * 任务页（/tasks）文案与结构 —— 原卡 t_13a2b66b #10 的四条验收，在
 * DM-B2（KI-37，用户裁决 2026-10-25）把「四段堆叠」改成**统一 job 列表 + 三级下钻**后重新落位：
 *
 *  1) 页头讲清保留口径，但**先说清判定结果一直都在**（旧文案误伤判定结果的信任）；
 *  2) 不再有「导入批次 / 导出记录」两块固定分区 —— 它们是同一张 job 列表里的行，
 *     点开才看那一次的明细（L2）；
 *  3) 「最近一次」摘要只在导出那一次的明细里出现，且一句话说完（不重复成块）；
 *  4) 旧口径文案 `没有历史表` 在页面渲染与源码里都不再出现（grep 无命中）。
 */

const REPORT = {
  total: 20,
  exported: 12,
  skipped: [{ assetId: 7, fileName: 'a.jpg', reason: '文件已失联' }],
  errors: [],
  cancelled: false,
  dest: 'D:\\export\\2026-10-20',
  manifestWritten: true,
}

const { apiMock, state } = vi.hoisted(() => ({
  state: {
    // 每个用例可改：null = 没有报告（空态）
    report: null as Record<string, unknown> | null,
  },
  apiMock: {
    complianceJobs: vi.fn(),
    complianceReview: vi.fn(),
    exportStatus: vi.fn(),
    importBatches: vi.fn(),
    capabilities: vi.fn(),
    exportCancel: vi.fn(),
    openPath: vi.fn(),
    copyPath: vi.fn(),
    thumbUrl: (id: number, size = 256) => `/api/media/thumb/${id}?size=${size}`,
  },
}))

vi.mock('../src/lib/api', () => ({
  api: apiMock,
  artifactUrl: (path: string) => `/api/compliance/artifact?path=${encodeURIComponent(path)}`,
  DEFAULT_TAG_MAX_TOKENS: 2048,
  formatSize: (n: number) => `${n} B`,
}))

vi.mock('../src/lib/tasks', () => ({
  useTasks: () => ({
    tasks: [],
    runningCount: 0,
    startEmbed: vi.fn(),
    startImport: vi.fn(),
    startTag: vi.fn(),
    cancel: vi.fn(),
    retry: vi.fn(),
    retryFailed: vi.fn(),
    dismiss: vi.fn(),
  }),
  TASK_KIND_LABEL: {},
}))

vi.mock('../src/lib/toast', () => ({
  useToast: () => ({ toast: vi.fn(), dismiss: vi.fn() }),
  ToastProvider: ({ children }: { children: unknown }) => children,
}))

import { Tasks } from '../src/pages/Tasks'

// i18n-C（卡 t_06629257）：本文件断言的是**中文界面原文**，而迁移后文案走 t()、
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

beforeEach(() => {
  vi.clearAllMocks()
  state.report = REPORT
  apiMock.complianceJobs.mockResolvedValue({ jobs: [], total: 0 })
  apiMock.exportStatus.mockImplementation(async () => ({
    running: false,
    progress: null,
    report: state.report,
    error: null,
  }))
  apiMock.importBatches.mockResolvedValue([
    {
      batchId: 41,
      rootPath: 'D:\\照片\\2026-10',
      storeMode: 'copyin',
      status: 'done',
      totalFiles: 120,
      doneFiles: 120,
      error: null,
      startedAt: 1750000000,
      endedAt: 1750000060,
    },
  ])
  apiMock.complianceReview.mockResolvedValue({ packId: 'ecommerce', total: 3, threshold: 0.7, lowConfidenceOnly: true, items: [] })
  apiMock.capabilities.mockResolvedValue({
    isDesktop: false,
    storeMode: 'server-volume',
    writableRoots: [],
    roots: [],
    features: {
      moveAsset: false,
      renameAsset: false,
      relink: false,
      openInOS: false,
      physicalDelete: false,
      copyInImport: false,
      scanExternalPath: false,
    },
    featureReasons: {},
  })
})

afterEach(() => cleanup())

function renderTasks() {
  return render(
    <MemoryRouter initialEntries={['/tasks']}>
      <Tasks />
    </MemoryRouter>,
  )
}

describe('任务页文案与结构（DM-B2 / KI-37 复核原卡 t_13a2b66b #10）', () => {
  it('页头讲清保留口径，且先说清判定结果一直都在（不吓人）', async () => {
    renderTasks()
    const note = await waitFor(() => {
      const el = document.querySelector('[data-testid="tasks-retention-note"]')
      if (!el) throw new Error('页头保留口径说明没渲染')
      return el as HTMLElement
    })
    expect(note.textContent).toContain('一直都在')
    expect(note.textContent).toContain('最近 50 次')
    expect(note.textContent).toContain('最近一次')
    // 旧口径：把「只保留最近一次」直接扣在判定结果上 —— 误伤信任，必须不再出现
    expect(note.textContent).not.toContain('只保留最近一次')
  })

  it('导入 / 导出不再是两块固定分区：它们是同一张 job 列表里的行', async () => {
    renderTasks()
    const list = await waitFor(() => {
      const el = document.querySelector('[data-testid="job-list"]')
      if (!el) throw new Error('统一 job 列表没渲染')
      return el as HTMLElement
    })
    // 老分区载体必须消失（能力搬进 L2，而不是删掉）
    expect(document.querySelector('[data-testid="import-batches"]')).toBeNull()
    expect(document.querySelector('[data-testid="export-record"]')).toBeNull()
    expect(list.querySelector('[data-testid="job-row-import:41"]')).toBeTruthy()
    expect(list.querySelector('[data-testid="job-row-export:last"]')).toBeTruthy()
  })

  it('「最近一次」只在导出那一次的明细里出现（摘要一句话说完，不重复成块）', async () => {
    renderTasks()
    fireEvent.click(await waitFor(() => {
      const el = document.querySelector('[data-testid="job-row-export:last"]')
      if (!el) throw new Error('导出行没渲染')
      return el as HTMLElement
    }))
    const summary = await waitFor(() => {
      const el = document.querySelector('[data-testid="export-last-summary"]')
      if (!el) throw new Error('导出摘要没渲染')
      return el as HTMLElement
    })
    await waitFor(() => expect(summary.textContent).toContain('最近一次：导出 12/20 张'))
    expect(summary.textContent).toContain('跳过 1 张')
    // 摘要落在导出明细里、在明细块之前（职责单一：摘要归顶，明细归下）
    const detail = await waitFor(() => {
      const el = document.querySelector('[data-testid="export-report"]')
      if (!el) throw new Error('导出明细没渲染')
      return el as HTMLElement
    })
    expect(summary.compareDocumentPosition(detail) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy()
    expect(detail.textContent).not.toContain('最近一次')
    expect(detail.textContent).toContain('导出目标')
    // 页面上「最近一次」只出现在导出这一次的明细里（不再另起一块）
    expect((document.body.textContent ?? '').match(/最近一次：导出/g) ?? []).toHaveLength(1)
  })

  it('从没导出过就不编造空的导出记录（只在真要回看时才出现这一行）', async () => {
    state.report = null
    renderTasks()
    // 列表取数完成（导入批次已回来）后再判：导出既没报告也没在跑 → 不立空行
    await waitFor(() => expect(apiMock.importBatches).toHaveBeenCalled())
    await waitFor(() => expect(document.querySelector('[data-testid="job-row-import:41"]')).toBeTruthy())
    expect(document.querySelector('[data-testid="job-row-export:last"]')).toBeNull()
    // 用户仍然知道导出报告的保留口径（页头说清）
    expect(document.querySelector('[data-testid="tasks-retention-note"]')?.textContent).toContain('导出报告留最近一次')
  })

  it('旧口径文案「没有历史表」彻底消失（渲染 + 源码双检）', async () => {
    renderTasks()
    await waitFor(() => expect(apiMock.exportStatus).toHaveBeenCalled())
    await waitFor(() => expect(apiMock.importBatches).toHaveBeenCalled())
    expect(document.body.textContent ?? '').not.toContain('没有历史表')

    const src = readFileSync(resolve(__dirname, '../src/pages/Tasks.tsx'), 'utf8')
    expect(src).not.toContain('没有历史表')
  })
  it('DECouple-FE：复核队列入口只在任务中心（低频 Dialog），不再是独立页', async () => {
    renderTasks()
    const btn = await waitFor(() => {
      const el = document.querySelector('[data-testid="tasks-review-queue"]')
      if (!el) throw new Error('待人工看入口没渲染')
      return el as HTMLElement
    })
    // 计数来自 /api/compliance/review 的 total（跨 job 的待人工看条目）
    await waitFor(() => expect(btn.textContent).toContain('3'))
    fireEvent.click(btn)
    // 点开 = 低频 Dialog（不是路由跳转）：URL 带 ?dialog=review-queue，页面不换
    await waitFor(() => expect(document.querySelector('[data-testid="review-queue-dialog"]')).toBeTruthy())
  })
})
