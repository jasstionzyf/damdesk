// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, render, waitFor } from '@testing-library/react'
import { MemoryRouter } from 'react-router-dom'

/**
 * i18n-C（卡 task）：**英文界面零残留中文** 的硬门禁。
 *
 * 前面那些 dom 用例钉 zh-Hans 验「中文原文没被翻译偷改」；本文件反过来把语言钉 en-US，
 * 把处理与结果面的每个界面渲染出来，扫全 DOM 文本里的**汉字串**，必须为空
 * （白名单只允许「此刻还归别人管」的中文：
 *   - 用户数据（文件名/标签/路径）—— 夹具里一律用英文，故实际不需要；
 *   - lib 层判据标签（`lib/compliance.ts` 的 stateLabel、`lib/pluginResult.ts` 的
 *     VERDICT_LABELS）—— 属 D 卡（设置页 + lib 层）范围，本卡不动 lib，
 *     D 卡落库后下面 `LIB_PENDING` 应清空）。
 *
 * 为什么值得单独一条用例：组件里漏一个 `t()`，中文界面完全看不出来（原文就是中文），
 * 只有切到英文才暴露；这条用例就是那个「切到英文」的机器判据。
 */

// 语言钉 en-US：必须在所有 import 之前（i18n 模块初始化时读一次 localStorage 定初值）
vi.hoisted(() => {
  // jsdom 没有 EventSource（去重页订阅进度用）；给个空壳
  vi.stubGlobal(
    'EventSource',
    class {
      close() {}
      addEventListener() {}
      removeEventListener() {}
    },
  )
  try {
    localStorage.setItem('dam.locale', 'en-US')
  } catch {
    vi.stubGlobal('localStorage', {
      getItem: (k: string) => (k === 'dam.locale' ? 'en-US' : null),
      setItem: () => {},
      removeItem: () => {},
      clear: () => {},
      key: () => null,
      length: 0,
    })
  }
})

const apiStub = vi.hoisted(() => ({
  appInfo: vi.fn(),
  capabilities: vi.fn(),
  specs: vi.fn(),
  startJob: vi.fn(),
  complianceJobs: vi.fn(),
  importBatches: vi.fn(),
  exportStatus: vi.fn(),
  exportStart: vi.fn(),
  exportCancel: vi.fn(),
  assets: vi.fn(),
  dedupTrash: vi.fn(),
  dedupRestore: vi.fn(),
  dedupPurge: vi.fn(),
  dupGroups: vi.fn(),
  dedupResolve: vi.fn(),
  dedupIgnore: vi.fn(),
  dedupUnignore: vi.fn(),
  dedupExportUrl: vi.fn(),
  thumbUrl: vi.fn((id: number) => `/api/assets/${id}/thumb`),
  rs: vi.fn(),
}))

vi.mock('../src/lib/api', async (importOriginal) => {
  // 只替换 api 句柄，其余工具函数（artifactUrl/formatSize/...）保持真身：
  // 整模块替换会让组件里 import 的辅助函数变 undefined → 「type is invalid」假报错。
  const actual = await importOriginal<typeof import('../src/lib/api')>()
  return { ...actual, api: apiStub, formatSize: (n: number) => `${n} B` }
})

import { ConfirmProvider } from '../src/lib/confirm'
import { TaskCenterProvider } from '../src/lib/tasks'
import { ToastProvider } from '../src/lib/toast'
import { Tasks } from '../src/pages/Tasks'
import { Trash } from '../src/pages/Trash'
import { Dedup } from '../src/pages/Dedup'
import { Process } from '../src/pages/Process'
import { ResultTable } from '../src/components/ResultTable'
import { ExportDialog } from '../src/components/ExportDialog'
import { ScanReportView } from '../src/components/DedupScanDialog'

/** lib 层仍在中文的判据标签（D 卡范围；本卡只保证「本卡负责的界面文案」全英文） */
const LIB_PENDING = [
  // lib/compliance.ts :: stateLabel()
  '合格',
  '不合格',
  '未判定',
  '待人工看',
  // lib/pluginResult.ts :: VERDICT_LABELS
  '通过',
  '不通过',
  '待复核',
  '已改判',
  '无法判定',
  // lib/resultColumns.ts :: 列标签与执行结果标签（表格列名由 lib 生成，不是本卡 30 个文件里的文案）
  '资产',
  '执行结果',
  '产物',
  '备注',
  '已执行',
  '跳过',
  // lib/complianceJobs.ts :: 执行状态文案
  '已完成',
  // lib/specs.ts :: assetSetOptions() 的图集选项文案
  '选中的',
  '张',
  '先在图库选图',
]

/** 全 DOM 里的汉字串（不是「中文标点」——标点不算文案） */
function cjkRuns(root: ParentNode): string[] {
  const text = (root as HTMLElement).textContent ?? ''
  return text.match(/[\u4e00-\u9fff]+/g) ?? []
}

/** 断言：除白名单外没有任何汉字串，失败时把「残留串 + 上下文」打出来便于定位文件 */
function expectNoChinese(label: string, root: ParentNode) {
  const runs = cjkRuns(root)
  const leftover = runs.filter((r) => !LIB_PENDING.some((w) => r.includes(w) || w.includes(r)))
  if (leftover.length) {
    const text = (root as HTMLElement).textContent ?? ''
    const detail = [...new Set(leftover)]
      .map((r) => {
        const i = text.indexOf(r)
        const ctx = text.slice(Math.max(0, i - 40), i + r.length + 40).replace(/\s+/g, ' ')
        return `  · "${r}"  …${ctx}…`
      })
      .join('\n')
    throw new Error(`${label}: 英文界面残留中文 ${leftover.length} 处\n${detail}`)
  }
  expect(leftover).toEqual([])
}

beforeEach(() => {
  apiStub.appInfo.mockResolvedValue({ dataDir: '/data' })
  apiStub.capabilities.mockResolvedValue({ features: {} })
  apiStub.specs.mockResolvedValue({ items: [] })
  // 真实返回形状：complianceJobs = { jobs, total }；importBatches = 数组；dupGroups = { groups, ... }
  apiStub.complianceJobs.mockResolvedValue({ jobs: [], total: 0 })
  apiStub.importBatches.mockResolvedValue([])
  apiStub.exportStatus.mockResolvedValue({ running: false, progress: null, report: null, error: null })
  apiStub.assets.mockResolvedValue({ items: [], total: 0 })
  apiStub.dedupTrash.mockResolvedValue({ items: [], total: 0 })
  apiStub.dupGroups.mockResolvedValue({
    totalGroups: 0,
    totalAssets: 0,
    reclaimBytes: 0,
    kindCounts: {},
    groups: [],
  })
  apiStub.rs.mockReturnValue(null as never)
})

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
})

function renderAt(ui: React.ReactElement, path = '/') {
  return render(
    <ToastProvider>
      <ConfirmProvider>
        <TaskCenterProvider>
          <MemoryRouter initialEntries={[path]}>{ui}</MemoryRouter>
        </TaskCenterProvider>
      </ConfirmProvider>
    </ToastProvider>,
  )
}

describe('英文界面零残留中文（i18n-C 门禁）', () => {
  it('任务中心：页头/列表分区/导出记录/空态全英文', async () => {
    const { container } = renderAt(<Tasks />, '/tasks')
    await waitFor(() => expect(apiStub.exportStatus).toHaveBeenCalled())
    // 列表/页头渲染完再扫（loading 态页面是空的，扫空页面等于没测）
    await waitFor(() => expect((container.textContent ?? '').length).toBeGreaterThan(80))
    expectNoChinese('任务中心', container)
  })

  it('处理页：四个动作 tab + 说明 + 空态全英文', async () => {
    const { container } = renderAt(<Process />, '/process')
    await waitFor(() => expect(apiStub.specs).toHaveBeenCalled())
    expectNoChinese('处理页', container)
  })

  it('去重页 + 重复扫描报告（含报告 note/表头/按钮）全英文', async () => {
    const report = {
      scanId: 'scan-1',
      path: '/data/in',
      scanned: 120,
      truncated: false,
      pendingFiles: 0,
      unreadable: [],
      totalGroups: 4,
      reclaimBytes: 1024,
      kindCounts: { exact: 2, near: 2 },
      note: 'read-only scan: nothing written to the library',
      groups: [],
    }
    const { container } = renderAt(
      <div>
        <Dedup />
        <ScanReportView report={report as never} />
      </div>,
      '/dedup',
    )
    await waitFor(() => expect(apiStub.dupGroups).toHaveBeenCalled())
    expectNoChinese('去重页', container)
  })

  it('导出面板（范围/内容/打包/目标路径）全英文', async () => {
    renderAt(<ExportDialog />, '/?dialog=export&ids=3,9')
    await waitFor(() => expect(document.body.textContent ?? '').toContain('Start export'))
    expectNoChinese('导出面板', document.body)
  })

  it('检查结果表（表头/判据列）全英文', async () => {
    const rows = [
      {
        assetId: 3,
        fileName: 'shoe-main.jpg',
        ruleId: 'amazon-main-image',
        result: 'ok',
        verdict: { status: 'pass', confidence: 0.93, reasonCode: null },
        reason: 'size 1200x1200 ok',
      },
    ]
    const { container } = renderAt(
      <div>
        <ResultTable rows={rows as never} total={1} />
      </div>,
      '/tasks',
    )
    await waitFor(() => expect(container.textContent).toContain('shoe-main.jpg'))
    expectNoChinese('检查结果表', container)
  })

  it('回收站空态与按钮全英文', async () => {
    const { container } = renderAt(<Trash />, '/trash')
    await waitFor(() => expect(apiStub.dedupTrash).toHaveBeenCalled())
    expectNoChinese('回收站', container)
  })
})
