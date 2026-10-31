// @vitest-environment jsdom
/**
 * KI35-P3（task）：能力面板（送去处理 Dialog）的**渲染与提交链路**（mock api）。
 *
 * 验收点（卡 task）：
 *  1. 图库选 2 张 → 选「合规检查」+ 一条方案 → POST /api/jobs（specId + assetIds）
 *     → 跳「处理记录」页（/tasks?job=runId）看进度；
 *  2. 能力行的动词词（检查/处理）由 manifest kinds 决定；方案分「官方默认 / 我的方案」；
 *  3. 处理类不给「全部图库」范围（一键改全库红线）；判定类给；
 *  4. 「当前筛选结果」范围：读数与执行用同一份条件（先 count 再回读 ids）；
 *  5. 没装任何能力 → 面板给安装引导（不是空面板）。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter, Route, Routes, useLocation } from 'react-router-dom'

import { SendToProcessDialog } from '../src/components/SendToProcessDialog'
import { VerbActions } from '../src/components/VerbActions'
import type { ComplianceInstalledPluginPack, SpecView } from '../src/lib/api'
import { ToastProvider } from '../src/lib/toast'
import { stashProcessFilterScope } from '../src/lib/sendToProcess'

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

const apiStub = vi.hoisted(() => ({
  compliancePacks: vi.fn(),
  specs: vi.fn(),
  assets: vi.fn(),
  startJob: vi.fn(),
}))
vi.mock('../src/lib/api', () => ({ api: apiStub }))

const EC = 'com.soujpg.pack.ecommerce'
const SEG = 'com.soujpg.pack.segment'

/** 接口真实形态（DM-B1）：名字/描述随包下发（manifest `short_name` / `summary`） */
const DISPLAY: Record<string, { name: string; summary: string }> = {
  [EC]: { name: '合规检查', summary: '检查图片能不能直接用在电商平台（淘宝 / Amazon）上' },
  [SEG]: { name: '抠图换背景', summary: '把图片里的主体抠出来，换成纯色或新背景' },
}

function pack(id: string, kinds: string[], over: Partial<ComplianceInstalledPluginPack> = {}): ComplianceInstalledPluginPack {
  return {
    builtin: true,
    ...DISPLAY[id],
    currentVersion: '1.0.0',
    staleCurrent: null,
    versions: [
      {
        version: '1.0.0',
        dir: `/packs/${id}/1.0.0`,
        present: true,
        manifestHash: null,
        apiVersion: '1',
        deterministic: true,
        signatureStatus: 'unsigned',
        installedAt: 0,
        current: true,
        registered: true,
        kinds,
      },
    ],
    runtime: { available: true },
    ...over,
    id,
  }
}

function spec(specId: number, over: Partial<SpecView> = {}): SpecView {
  return {
    specId,
    pluginId: EC,
    pluginVersionPin: null,
    name: `方案 ${specId}`,
    industry: null,
    rules: ['rule.a'],
    thresholds: {},
    actions: [],
    output: {},
    paramsDiff: {},
    enabled: 1,
    originKey: null,
    createdAt: 0,
    updatedAt: 0,
    pluginStatus: {
      installed: true,
      pluginName: DISPLAY[EC].name,
      pluginSummary: DISPLAY[EC].summary,
      version: '1',
      versionMismatch: false,
      paramsSchemaDeclared: true,
      rulesAvailable: ['rule.a'],
      note: null,
    },
    ...over,
  }
}

function LocationProbe() {
  const loc = useLocation()
  return <div data-testid="loc">{`${loc.pathname}${loc.search}`}</div>
}

function renderDialog(url = '/?dialog=process&ids=1,2') {
  return render(
    <ToastProvider>
      <MemoryRouter initialEntries={[url]}>
        <Routes>
          <Route path="/" element={<div data-testid="gallery-page">gallery</div>} />
          <Route path="/tasks" element={<div data-testid="tasks-landing">tasks</div>} />
        </Routes>
        <LocationProbe />
        <SendToProcessDialog />
      </MemoryRouter>
    </ToastProvider>,
  )
}

beforeEach(() => {
  apiStub.compliancePacks.mockReset()
  apiStub.specs.mockReset()
  apiStub.assets.mockReset()
  apiStub.startJob.mockReset()
  // 面板打开即读一遍全量方案（能力清单补集 + seeding）；缺省 = 空列表，单测各自覆盖
  apiStub.specs.mockResolvedValue({ total: 0, items: [] })
  sessionStorage.clear()
})
afterEach(() => cleanup())

describe('验收 1：选 2 张 → 合规检查 + 方案 → 建 job → 跳处理记录', () => {
  it('全链路：能力行有动词词 → 官方默认/我的方案分组 → startJob(specId, assetIds) → /tasks?job=', async () => {
    apiStub.compliancePacks.mockResolvedValue({ plugins: [pack(EC, ['judge'])] })
    apiStub.specs.mockResolvedValue({
      total: 2,
      items: [
        spec(11, { originKey: 'seed:ecommerce-amazon', name: '亚马逊主图' }),
        spec(12, { originKey: null, name: '我的 800px webp' }),
      ],
    })
    apiStub.startJob.mockResolvedValue({ ok: true, runId: 41, queuedAssets: 2, candidates: 2, queuedGroups: 0 })
    renderDialog()

    // 能力行：名称 + 动词词（judge → 检查）+ 一句话描述
    const row = await screen.findByTestId(`send-process-cap-${EC}`)
    expect(row.textContent).toContain('合规检查')
    expect(screen.getByTestId(`send-process-cap-verb-${EC}`).textContent).toBe('检查')
    expect(row.textContent).toContain('电商平台')

    // 选能力 → 拉这个能力下的方案；两条不自动选（必须显式选）
    fireEvent.click(screen.getByTestId(`send-process-cap-input-${EC}`))
    await waitFor(() => expect(apiStub.specs).toHaveBeenCalledWith(EC))
    const groups = await screen.findByTestId('send-process-plans')
    expect(groups.textContent).toContain('官方默认')
    expect(groups.textContent).toContain('我的方案')
    expect(groups.textContent).toContain('亚马逊主图')
    expect(groups.textContent).toContain('我的 800px webp')

    // 未选方案：确认按钮灰（此时动词未定，按钮文案就是「开始」）
    expect((screen.getByText('开始') as HTMLButtonElement).disabled).toBe(true)

    // 选「我的方案」→ 确认按钮动词是检查（无动作链）
    fireEvent.click(screen.getByTestId('send-process-plan-12'))
    expect(screen.getByText('开始检查')).toBeTruthy()

    // 范围默认「当前选中 2 张」；判定类给「全部图库」
    const selLabel = screen.getByTestId('send-process-range-selection').closest('label') as HTMLElement
    expect(selLabel.textContent).toContain('当前选中的 2 张')
    expect(screen.getByTestId('send-process-range-all')).toBeTruthy()

    // 提交 → POST /api/jobs 只带 specId + assetIds（不带 all）
    fireEvent.click(screen.getByText('开始检查'))
    await waitFor(() => expect(apiStub.startJob).toHaveBeenCalled())
    expect(apiStub.startJob).toHaveBeenCalledWith({ specId: 12, assetIds: [1, 2], all: undefined })
    // 跳「处理记录」页，带这一次的记录号
    await waitFor(() => expect(screen.getByTestId('loc').textContent).toBe('/tasks?job=41'))
  })

  it('只有一条可用方案 → 默认选中（用户少点一次）', async () => {
    apiStub.compliancePacks.mockResolvedValue({ plugins: [pack(EC, ['judge'])] })
    apiStub.specs.mockResolvedValue({ total: 1, items: [spec(9, { originKey: 'seed:ecommerce' })] })
    renderDialog()
    fireEvent.click(await screen.findByTestId(`send-process-cap-input-${EC}`))
    await waitFor(() => expect((screen.getByTestId('send-process-plan-9') as HTMLLabelElement).textContent).toBeTruthy())
    // 唯一方案自动选中 → 确认按钮直接可点
    await waitFor(() => expect((screen.getByText('开始检查') as HTMLButtonElement).disabled).toBe(false))
  })
})

describe('验收 2/3：动词词与范围门禁（kinds 决定）', () => {
  it('处理类能力：动词词=处理、方案有动作链 → 按钮开始处理；范围没有「全部图库」', async () => {
    apiStub.compliancePacks.mockResolvedValue({ plugins: [pack(EC, ['judge']), pack(SEG, ['process'])] })
    apiStub.specs.mockResolvedValue({
      total: 1,
      items: [spec(21, { pluginId: SEG, name: '纯白背景', actions: [{ action: 'segment' }, { action: 'white_bg_fill' }] })],
    })
    apiStub.startJob.mockResolvedValue({ ok: true, runId: 42, queuedAssets: 2, candidates: 2, queuedGroups: 0 })
    renderDialog()

    expect((await screen.findByTestId(`send-process-cap-verb-${SEG}`)).textContent).toBe('处理')
    fireEvent.click(screen.getByTestId(`send-process-cap-input-${SEG}`))
    await waitFor(() => expect(apiStub.specs).toHaveBeenCalledWith(SEG))
    // 唯一方案自动选中；有动作链 → 按钮「开始处理」
    await waitFor(() => expect(screen.getByText('开始处理')).toBeTruthy())
    expect(screen.queryByTestId('send-process-range-all')).toBeNull()

    fireEvent.click(screen.getByText('开始处理'))
    await waitFor(() => expect(apiStub.startJob).toHaveBeenCalledWith({ specId: 21, assetIds: [1, 2], all: undefined }))
  })
})

describe('验收 4：「当前筛选结果」范围 —— 读数与执行同一份条件', () => {
  it('先 count（limit=1）再回读 ids（limit=500），startJob 收到回读的 assetIds', async () => {
    apiStub.compliancePacks.mockResolvedValue({ plugins: [pack(EC, ['judge'])] })
    apiStub.specs.mockResolvedValue({ total: 1, items: [spec(31, { originKey: 'seed:ecommerce' })] })
    apiStub.startJob.mockResolvedValue({ ok: true, runId: 43, queuedAssets: 2, candidates: 2, queuedGroups: 0 })
    stashProcessFilterScope({ label: '图库当前筛选', filter: { tag: 'cat' } })
    // count（limit=1）与回读（limit=500）走同一个桩：读数与执行载荷天然同源
    apiStub.assets.mockImplementation(async (q: { limit?: number }) =>
      q.limit === 1 ? { total: 2, items: [] } : { total: 2, items: [{ assetId: 7 }, { assetId: 8 }] },
    )
    renderDialog()

    // 打开面板即用这份条件数一遍（DT4：界面读数与执行载荷同源）
    await waitFor(() =>
      expect(apiStub.assets).toHaveBeenCalledWith(expect.objectContaining({ tag: 'cat', limit: 1 })),
    )

    fireEvent.click(await screen.findByTestId(`send-process-cap-input-${EC}`))
    await waitFor(() => expect((screen.getByText('开始检查') as HTMLButtonElement).disabled).toBe(false))
    const filterLabel = screen.getByTestId('send-process-range-filter').closest('label') as HTMLElement
    expect(filterLabel.textContent).toContain('图库当前筛选')
    expect(filterLabel.textContent).toContain('2 张')

    fireEvent.click(screen.getByTestId('send-process-range-filter'))
    fireEvent.click(screen.getByText('开始检查'))
    await waitFor(() => expect(apiStub.startJob).toHaveBeenCalled())
    expect(apiStub.startJob).toHaveBeenCalledWith({ specId: 31, assetIds: [7, 8], all: undefined })
  })
})

describe('验收 5：没装任何能力 → 安装引导', () => {
  it('注册表读到 0 个 → 引导去设置装包（不是空面板）', async () => {
    apiStub.compliancePacks.mockResolvedValue({ plugins: [] })
    renderDialog()
    const guide = await screen.findByTestId('send-process-caps-empty')
    expect(guide.textContent).toContain('还没有可用的处理功能')
    const link = screen.getByRole('link', { name: '设置' })
    expect(link.getAttribute('href')).toBe('/settings?drawer=packs')
  })
})

describe('验收 1（fresh 库形态）：注册表还没有内置包的行，方案清单补集照样给出「合规检查」', () => {
  it('plugins=[] + 官方默认方案已 seeding → 能力行出现（动词=检查）→ 选方案 → 建 job → 跳处理记录', async () => {
    // 实测形态：内置判定包第一次跑判定前不进注册表，但 GET /api/specs 已能看到官方默认方案
    apiStub.compliancePacks.mockResolvedValue({ plugins: [] })
    apiStub.specs.mockImplementation(async () => ({
      total: 1,
      items: [spec(51, { originKey: 'legacy-pack:ecommerce-taobao', name: '淘宝主图' })],
    }))
    apiStub.startJob.mockResolvedValue({ ok: true, runId: 55, queuedAssets: 2, candidates: 2, queuedGroups: 0 })
    renderDialog()

    const row = await screen.findByTestId(`send-process-cap-${EC}`)
    expect(row.textContent).toContain('合规检查')
    expect(screen.getByTestId(`send-process-cap-verb-${EC}`).textContent).toBe('检查')
    expect(screen.queryByTestId('send-process-caps-empty')).toBeNull()

    fireEvent.click(screen.getByTestId(`send-process-cap-input-${EC}`))
    // 单方案自动选中 → 直接提交
    await waitFor(() => expect((screen.getByText('开始检查') as HTMLButtonElement).disabled).toBe(false))
    fireEvent.click(screen.getByText('开始检查'))
    await waitFor(() =>
      expect(apiStub.startJob).toHaveBeenCalledWith({ specId: 51, assetIds: [1, 2], all: undefined }),
    )
    await waitFor(() => expect(screen.getByTestId('loc').textContent).toBe('/tasks?job=55'))
  })
})

describe('入口按钮（VerbActions）：打开面板并把选择集带进 URL', () => {
  it('点「送去处理」→ ?dialog=process&ids=…；没选择集灰置', async () => {
    apiStub.compliancePacks.mockResolvedValue({ plugins: [pack(EC, ['judge'])] })
    render(
      <ToastProvider>
        <MemoryRouter initialEntries={['/']}>
          <Routes>
            <Route path="/" element={<div data-testid="gallery-page">gallery</div>} />
          </Routes>
          <LocationProbe />
          <VerbActions ids={[3, 5]} />
          <SendToProcessDialog />
        </MemoryRouter>
      </ToastProvider>,
    )
    const btn = screen.getByTestId('verb-send')
    expect(btn.textContent).toContain('送去处理')
    expect(btn.getAttribute('title')).toContain('2 张')
    fireEvent.click(btn)
    // useOverlay 走 URLSearchParams：逗号编码成 %2C（读取侧 parseIdsParam 按解码值拆分）
    await waitFor(() => expect(screen.getByTestId('loc').textContent).toBe('/?dialog=process&ids=3%2C5'))
    // 面板标题带选择集张数；寄存过空筛选上下文 → 没有「当前筛选结果」范围
    await screen.findByTestId('send-process-dialog')
    expect(screen.getByTestId('send-process-dialog').textContent).toContain('选中的 2 张')
    expect(screen.queryByTestId('send-process-range-filter')).toBeNull()
  })
})

/**
 * KI-39（task）：**处理类能力从面板发得动**。
 *
 * 真机现象：图库选图 → 送去处理 → 抠图/换背景，「官方默认」分组**空**、自己新建一条又不带
 * 动作链 → run 静默落成电商判定 run、结果页空。后端现在为「恰好声明一个动作」的处理类包
 * 派生官方默认方案（`seed:<pack>`，动作链 `[white_bg_fill]`）。这里钉死界面侧：
 * 注册表还没这一行（fresh 库形态）时，方案清单照样把「抠图换背景」补成能力行，
 * 动词是**处理**、「官方默认」里能选到「抠图白底」，点一下就把这条方案的 specId 发出去。
 */
describe('KI-39：处理类官方默认方案在面板里可选可发（fresh 库）', () => {
  it('plugins=[] + 段包官方默认方案 → 能力行「处理」→ 官方默认有「抠图白底」→ startJob(specId)', async () => {
    apiStub.compliancePacks.mockResolvedValue({ plugins: [] })
    apiStub.specs.mockImplementation(async () => ({
      total: 1,
      items: [
        spec(101, {
          pluginId: SEG,
          name: '抠图白底',
          originKey: `seed:${SEG}`,
          rules: [],
          thresholds: {},
          actions: [{ action: 'white_bg_fill' }],
          pluginStatus: {
            installed: true,
            pluginName: DISPLAY[SEG].name,
            pluginSummary: DISPLAY[SEG].summary,
            version: '1.2.1',
            versionMismatch: false,
            paramsSchemaDeclared: true,
            rulesAvailable: [],
            note: null,
          },
        }),
      ],
    }))
    apiStub.startJob.mockResolvedValue({ ok: true, runId: 77, queuedAssets: 2, candidates: 2, queuedGroups: 0 })
    renderDialog()

    // 能力行由方案补出（注册表空）：名字/描述来自包，动词词 = 处理
    const row = await screen.findByTestId(`send-process-cap-${SEG}`)
    expect(row.textContent).toContain('抠图换背景')
    expect(screen.getByTestId(`send-process-cap-verb-${SEG}`).textContent).toBe('处理')

    fireEvent.click(screen.getByTestId(`send-process-cap-input-${SEG}`))
    await waitFor(() => expect(apiStub.specs).toHaveBeenCalledWith(SEG))
    // 「官方默认」不是空的：里面就是包里带来的「抠图白底」
    const groups = await screen.findByTestId('send-process-plans')
    expect(groups.textContent).toContain('官方默认')
    expect(groups.textContent).toContain('抠图白底')

    // 唯一方案自动选中 → 按钮「开始处理」；处理类没有「全部图库」
    await waitFor(() => expect((screen.getByText('开始处理') as HTMLButtonElement).disabled).toBe(false))
    expect(screen.queryByTestId('send-process-range-all')).toBeNull()

    fireEvent.click(screen.getByText('开始处理'))
    await waitFor(() => expect(apiStub.startJob).toHaveBeenCalledWith({ specId: 101, assetIds: [1, 2], all: undefined }))
    await waitFor(() => expect(screen.getByTestId('loc').textContent).toBe('/tasks?job=77'))
  })
})
