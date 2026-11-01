// @vitest-environment jsdom
/**
 * N12：处理页的接线（`pages/Process.tsx`）。
 *
 * 卡上的验收口径：「处理：选图集 → 选规格（含动作链）→ 跑 → 产物落工作版本」。
 * UI 层的可测部分就是这里：
 *  1. 图集来自图库带过来的 `?ids=`（本页不做第二套选图界面）；
 *  2. 只有一条处理规格时**默认选中**（不让用户再点）；
 *  3. 动作链按规格顺序展示（顺序即执行顺序）；
 *  4. 跑 = `POST /api/jobs {specId, assetIds}`（宿主单入口），request 形状必须精确；
 *  5. 没有处理类规格时**不给跑按钮兜底**，而是指到设置 › 方案目录。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter } from 'react-router-dom'

import { Process } from '../src/pages/Process'
import { ToastProvider } from '../src/lib/toast'
import { ConfirmProvider } from '../src/lib/confirm'
import type { SpecView } from '../src/lib/api'

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

const apiStub = vi.hoisted(() => ({
  specs: vi.fn(),
  startJob: vi.fn(),
  capabilities: vi.fn(),
}))
vi.mock('../src/lib/api', () => ({ api: apiStub, formatSize: (n: number) => `${n} B` }))

function spec(over: Partial<SpecView> & { specId: number }): SpecView {
  return {
    pluginId: 'com.example.imaging',
    pluginVersionPin: null,
    name: `规格 ${over.specId}`,
    industry: null,
    rules: [],
    thresholds: {},
    actions: [{ action: 'resize', params: { longEdge: 800 } }],
    output: {},
    paramsDiff: {},
    enabled: 1,
    originKey: null,
    createdAt: 0,
    updatedAt: 0,
    pluginStatus: {
      installed: true,
      version: '1.0.0',
      versionMismatch: false,
      paramsSchemaDeclared: true,
      rulesAvailable: [],
      note: null,
    },
    ...over,
  }
}

const ONE_PROCESS = spec({ specId: 3, name: '800px webp', actions: [{ action: 'resize' }, { action: 'convert' }] })

function renderPage(url = '/process?ids=11,12') {
  return render(
    <MemoryRouter initialEntries={[url]}>
      <ToastProvider>
        <ConfirmProvider>
          <Process />
        </ConfirmProvider>
      </ToastProvider>
    </MemoryRouter>,
  )
}

beforeEach(() => {
  apiStub.specs.mockResolvedValue({ total: 1, items: [ONE_PROCESS] })
  apiStub.startJob.mockResolvedValue({
    ok: true,
    packId: 'com.example.imaging',
    scope: 'asset',
    candidates: 2,
    queuedAssets: 2,
    queuedGroups: 0,
    runId: 42,
    workers: 1,
  })
})

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
})

describe('处理页：图集 → 规格 → 跑', () => {
  it('只有一条规格时默认选中，并展示动作链（顺序即执行顺序）', async () => {
    renderPage()
    // 单条 → 不做下拉（避免「只有一项的下拉」）
    expect(await screen.findByTestId('artifact-spec-auto')).toBeTruthy()
    expect(screen.queryByTestId('artifact-spec-select')).toBeNull()
    const steps = await screen.findByTestId('process-chain-steps')
    const items = Array.from(steps.querySelectorAll('li')).map((li) => li.textContent ?? '')
    expect(items[0]).toContain('resize')
    expect(items[1]).toContain('convert')
  })

  it('图集 = 带过来的这两张；点「开始处理」→ POST /api/jobs {specId, assetIds}', async () => {
    renderPage()
    const set = await screen.findByTestId('artifact-set-fixed')
    expect(set.textContent).toContain('2 张')

    fireEvent.click(screen.getByTestId('artifact-run'))
    // 动作链含改本体动作 → 先二次确认
    const confirmBtn = await screen.findByRole('button', { name: /处理 2 张/ })
    fireEvent.click(confirmBtn)

    await waitFor(() => expect(apiStub.startJob).toHaveBeenCalledTimes(1))
    expect(apiStub.startJob).toHaveBeenCalledWith({ specId: 3, assetIds: [11, 12] })
    // 跑完给出去任务中心看结果的入口（结果不在本页展开）
    const hint = await screen.findByTestId('artifact-result-hint')
    expect(hint.textContent).toContain('任务中心')
  })

  it('没带选择集 → 说清 0 张并给回图库的入口，不许直接开跑', async () => {
    renderPage('/process')
    expect(await screen.findByTestId('process-empty-set')).toBeTruthy()
    expect((screen.getByTestId('artifact-run') as HTMLButtonElement).disabled).toBe(true)
  })

  it('没有处理类规格 → 不给跑，指到设置 › 方案目录', async () => {
    // 只有检查类规格（无动作链）：处理页应当看到「还没有可用于处理的规格」
    apiStub.specs.mockResolvedValue({ total: 1, items: [spec({ specId: 5, actions: [] })] })
    renderPage()
    expect(await screen.findByTestId('artifact-specs-empty')).toBeTruthy()
    expect((screen.getByTestId('artifact-run') as HTMLButtonElement).disabled).toBe(true)
    expect(screen.getByTestId('artifact-specs-empty-hint').textContent).toContain('方案目录')
  })

  it('多条规格 → 交给用户选（不猜哪条是他想要的）', async () => {
    apiStub.specs.mockResolvedValue({
      total: 2,
      items: [spec({ specId: 3, name: 'A' }), spec({ specId: 4, name: 'B' })],
    })
    renderPage()
    const sel = (await screen.findByTestId('artifact-spec-select')) as HTMLSelectElement
    expect(sel.value).toBe('')
    fireEvent.change(sel, { target: { value: '4' } })
    expect(sel.value).toBe('4')
  })
})
