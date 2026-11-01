// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter, Route, Routes, useLocation } from 'react-router-dom'
import { ToastProvider } from '../src/lib/toast'
import { ConfirmProvider } from '../src/lib/confirm'
import { PluginDetail } from '../src/pages/PluginDetail'

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

/**
 * N4 前端验收（卡 t_d927e0d4）：插件详情页 = 一个插件唯一的配置面。
 *
 * 守的是设计口径 `plugin-design-and-ui.md` §4 + `plugin-param-schema.md` §9：
 *  T1 三段结构 + 顺序：能力说明（只读）→ 我的规格（可增删改）→ 参数默认值（只读）；
 *     能力说明必须给「干什么 / 要什么图 / 给我什么 / 引擎与依赖 / 版本与体积」；
 *  T2 未安装：schema 照样可见但控件灰态 + 「装好后可用」+ 去设置装包的入口；方案仍可见可编辑（卸载不删用户配置）；
 *  T3 新建方案：同一份 ParamForm，**只提交改过的键**（params_diff）；规则勾选进 rules；
 *  T4 outputs 差异：检查类才有「复核」字样与处理记录出口，处理类不提审核列；
 *  T5 页头两个固定动作：「新建方案」「先试一张」；试跑后**不跳走**，单张结果就地显示
 *     （KI-35 P2 起口径：判定类给 通过/未通过/待人工看 + 原因，处理类给产物缩略图；批量结果仍归处理记录）；
 *  T6 安装/切版本/回滚/卸载 只在这一页（不留两套插件页），且不出现批量进度/历史结果。
 *
 * 只有 `api` 是桩：路由、浮层、确认框、toast 全是真的。
 */

// ---- jsdom 存储桩（本仓 vitest 全局 environment=node，jsdom 用例里没有 localStorage）----
function memStorage(): Storage {
  const store = new Map<string, string>()
  return {
    getItem: (k: string) => store.get(k) ?? null,
    setItem: (k: string, v: string) => void store.set(k, String(v)),
    removeItem: (k: string) => void store.delete(k),
    clear: () => store.clear(),
    key: (i: number) => [...store.keys()][i] ?? null,
    get length() {
      return store.size
    },
  } satisfies Storage
}
Object.defineProperty(globalThis, 'localStorage', { value: memStorage(), configurable: true, writable: true })
Object.defineProperty(globalThis, 'sessionStorage', { value: memStorage(), configurable: true, writable: true })

const { apiMock } = vi.hoisted(() => ({
  apiMock: {
    compliancePacks: vi.fn(),
    compliancePluginPackDetail: vi.fn(),
    pluginParamsSchema: vi.fn(),
    specs: vi.fn(),
    specCreate: vi.fn(),
    specUpdate: vi.fn(),
    specDelete: vi.fn(),
    specTry: vi.fn(),
    // KI-35 P2：「先试一张」就地结果 = 这一次 run 的收尾轮询 + 结果行
    complianceJob: vi.fn(),
    complianceResults: vi.fn(),
    thumbUrl: vi.fn(),
    compliancePackActivateVersion: vi.fn(),
    compliancePackRollback: vi.fn(),
    compliancePackUninstall: vi.fn(),
    compliancePackInstall: vi.fn(),
  },
}))

vi.mock('../src/lib/api', () => ({ api: apiMock }))

const SCHEMA = {
  schemaVersion: 1,
  groups: [{ key: 'basic', title: '判定门槛' }],
  properties: {
    shortSideMin: { type: 'integer', title: '短边下限', min: 200, max: 8000, step: 100, default: 1600, unit: 'px', group: 'basic' },
    whiteMaxDev: { type: 'integer', title: '白底允许偏差', min: 0, max: 64, step: 1, default: 8, group: 'basic' },
  },
  required: [],
}

const PLUGIN_ID = 'com.soujpg.pack.ecommerce'

function packOf(over: Record<string, unknown> = {}) {
  return {
    id: PLUGIN_ID,
    builtin: true,
    currentVersion: '1.3.0',
    staleCurrent: null,
    versions: [
      {
        version: '1.3.0',
        dir: `/test-fixture/plugins/${PLUGIN_ID}/1.3.0`,
        present: true,
        manifestHash: 'a'.repeat(64),
        apiVersion: '1.1',
        deterministic: false,
        signatureStatus: 'declared',
        installedAt: 200,
        current: true,
      },
      {
        version: '1.2.0',
        dir: `/test-fixture/plugins/${PLUGIN_ID}/1.2.0`,
        present: true,
        manifestHash: 'b'.repeat(64),
        apiVersion: '1.1',
        deterministic: false,
        signatureStatus: 'declared',
        installedAt: 100,
        current: false,
      },
    ],
    runtime: { engine: 'plugin', available: true, id: PLUGIN_ID, version: '1.3.0' },
    ...over,
  }
}

function packsView(over: Record<string, unknown> = {}) {
  return {
    activePack: { source: 'builtin', id: 'ecommerce', activatedAt: 0 },
    builtin: [{ id: 'ecommerce', source: 'builtin', rules: 6, taxonomyFields: 6, steps: 3, error: null }],
    imported: [],
    plugins: [packOf()],
    signaturePolicy: { required: false, trustedKeys: 0 },
    hostCapabilities: ['io.read', 'vl.judge', 'op.geometric'],
    ...over,
  }
}

function manifestOf(outputs: string[] | undefined) {
  return {
    id: PLUGIN_ID,
    name: '电商主图合规（白底商品图 / 淘宝·Amazon 通用）',
    version: '1.3.0',
    api_version: '1.1',
    entrypoint: { cmd: 'python3', args: ['plugin/__init__.py'] },
    capabilities: ['io.read', 'vl.judge', 'op.geometric'],
    deterministic: false,
    timeout_secs: 90,
    ...(outputs ? { outputs } : {}),
    resources: ['resources/rules.json'],
    permissions: { read: true, net: false },
    params: 'resources/params.schema.json',
  }
}

function detailOf(outputs: string[] | undefined) {
  return {
    id: PLUGIN_ID,
    source: 'plugin' as const,
    builtin: true,
    active: true,
    currentVersion: '1.3.0',
    dir: `/test-fixture/plugins/${PLUGIN_ID}/1.3.0`,
    manifest: manifestOf(outputs),
    rules: [
      { id: 'R0_product_main', name: '领域 gate' },
      { id: 'R1_size', name: '尺寸达标' },
      { id: 'R2_white_bg', name: '白底图纯白' },
    ],
  }
}

function specOf(over: Record<string, unknown> = {}) {
  return {
    specId: 300,
    pluginId: PLUGIN_ID,
    pluginVersionPin: null,
    name: '淘宝主图',
    industry: 'ecommerce',
    rules: ['R1_size'],
    thresholds: { R1_size: { min: 700 } },
    actions: [],
    output: {},
    paramsDiff: { shortSideMin: 1800 },
    enabled: true,
    originKey: 'legacy-pack:ecommerce-taobao',
    createdAt: 1,
    updatedAt: 2,
    pluginStatus: {
      installed: true,
      version: '1.3.0',
      versionMismatch: false,
      paramsSchemaDeclared: true,
      rulesAvailable: ['R0_product_main', 'R1_size', 'R2_white_bg'],
      note: null,
    },
    ...over,
  }
}

/** 「先试一张」结果行（GET /api/compliance/results?runId=42 的单张行） */
function resultRowOf(over: Record<string, unknown> = {}) {
  return {
    assetId: 7,
    fileName: 'a.jpg',
    absPath: '/x/a.jpg',
    ruleId: 'R1_size',
    judgementScope: 'per_asset',
    result: 'ok',
    verdict: { status: 'pass', confidence: 0.93, reasonCode: 'R1_size' },
    reason: null,
    fields: {},
    ts: 3,
    ...over,
  }
}

function resultsViewOf(outputs: string[], items: unknown[]) {
  return {
    packId: PLUGIN_ID,
    outputs,
    jobId: null,
    runId: 42,
    total: items.length,
    limit: 50,
    offset: 0,
    columns: null,
    columnWarnings: [],
    items,
  }
}

function LocationSpy() {
  const loc = useLocation()
  return <div data-testid="loc">{`${loc.pathname}${loc.search}`}</div>
}

function renderDetail() {
  return render(
    <MemoryRouter initialEntries={[`/plugins/${PLUGIN_ID}`]}>
      <ToastProvider>
        <ConfirmProvider>
          {/* 与真实应用壳同构：页面在 [data-overlay-base] 内，浮层必须 portal 到 body 才不被 inert 锁死 */}
          <div data-overlay-base="">
            <Routes>
              <Route path="/plugins/:pluginId" element={<PluginDetail />} />
              {/* KI-35 P1：跑与看结果都归任务中心（旧 /compliance 已重定向到能力详情页） */}
              <Route path="/tasks" element={<div data-testid="tasks-page">任务中心</div>} />
              <Route path="/settings" element={<div data-testid="settings-page">设置</div>} />
            </Routes>
            <LocationSpy />
          </div>
        </ConfirmProvider>
      </ToastProvider>
    </MemoryRouter>,
  )
}

async function ready(outputs: string[] = ['verdict']) {
  apiMock.compliancePacks.mockImplementation(async () => packsView())
  apiMock.pluginParamsSchema.mockImplementation(async () => ({
    pluginId: PLUGIN_ID,
    version: '1.3.0',
    pluginHash: 'a'.repeat(64),
    declared: true,
    schemaFile: 'resources/params.schema.json',
    schema: SCHEMA,
  }))
  apiMock.specs.mockImplementation(async () => ({ total: 2, items: [specOf(), specOf({ specId: 301, name: '亚马逊主图', rules: [], paramsDiff: {}, originKey: 'seed:ecommerce-amazon' })] }))
  apiMock.compliancePluginPackDetail.mockImplementation(async () => detailOf(outputs))
  renderDetail()
  await screen.findByTestId('plugin-section-capability')
  await screen.findByTestId('spec-row-300')
}

beforeEach(() => {
  localStorage.clear()
  apiMock.specCreate.mockImplementation(async (req: Record<string, unknown>) =>
    specOf({ specId: 400, name: req.name, rules: req.rules, paramsDiff: req.paramsDiff, originKey: null }),
  )
  apiMock.specUpdate.mockImplementation(async (_id: number, req: Record<string, unknown>) =>
    specOf({ specId: 300, name: req.name ?? '淘宝主图', paramsDiff: req.paramsDiff ?? {} }),
  )
  apiMock.specDelete.mockImplementation(async (id: number) => ({ specId: id, deleted: true }))
  apiMock.specTry.mockImplementation(async () => ({ ok: true, runId: 42, queuedAssets: 1 }))
  // 「先试一张」收尾轮询：默认已收工，结果 = 同一张图三条规则的结论（三种状态各一）
  apiMock.complianceJob.mockImplementation(async () => ({
    run: { runId: 42, status: 'done', statusLabel: '已完成' },
    taskTotal: 1,
    limit: 1,
    offset: 0,
    tasks: [],
  }))
  apiMock.complianceResults.mockImplementation(async () =>
    resultsViewOf(['verdict'], [
      resultRowOf(),
      resultRowOf({ ruleId: 'R2_white_bg', verdict: { status: 'fail', confidence: 0.61, reasonCode: 'R2_white_bg' } }),
      resultRowOf({ ruleId: 'R0_product_main', verdict: { status: 'undetermined', confidence: 0.42, reasonCode: 'R0_product_main' } }),
    ]),
  )
  apiMock.thumbUrl.mockImplementation((assetId: number, size = 256) => `/api/media/thumb/${assetId}?size=${size}`)
  apiMock.compliancePackActivateVersion.mockImplementation(async () => ({
    ok: true, id: PLUGIN_ID, mode: 'activate', from: '1.3.0', to: '1.2.0', version: '1.2.0',
  }))
  apiMock.compliancePackRollback.mockImplementation(async () => ({
    ok: true, id: PLUGIN_ID, mode: 'rollback', from: '1.3.0', to: '1.2.0', version: '1.2.0',
  }))
  apiMock.compliancePackUninstall.mockImplementation(async () => ({
    ok: true, id: PLUGIN_ID, removed: ['1.2.0'], remaining: ['1.3.0'], currentVersion: '1.3.0', staleCurrent: null,
  }))
})

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
})

describe('插件详情页：三段结构', () => {
  it('能力说明给全五类信息（干什么/要什么图/给我什么/引擎与依赖/版本与体积）', async () => {
    await ready()
    expect(screen.getByTestId('plugin-detail-title').textContent).toContain('电商主图合规')
    for (const key of ['what', 'needs', 'gives', 'engine', 'version']) {
      expect(screen.getByTestId(`plugin-cap-${key}`)).toBeTruthy()
    }
    // 内容由 manifest / 规则清单推导（不是写死话术）：规则 id 与入口、能力都在
    expect(screen.getByTestId('plugin-cap-needs').textContent).toContain('R1_size')
    expect(screen.getByTestId('plugin-cap-engine').textContent).toContain('python3')
    expect(screen.getByTestId('plugin-cap-engine').textContent).toContain('vl.judge')
    // 版本与体积：版本真给，体积宿主没上报就明说，不拿 0 冒充
    expect(screen.getByTestId('plugin-cap-version').textContent).toContain('1.3.0')
    expect(screen.getByTestId('plugin-cap-version').textContent).toContain('未上报')
  })

  it('我的规格列出该插件下的规格（规则数 / 参数改动 / 迁移来源）', async () => {
    await ready()
    expect(screen.getByTestId('spec-name-300').textContent).toBe('淘宝主图')
    expect(screen.getByTestId('spec-row-301').textContent).toContain('全部')
    expect(screen.getByTestId('spec-row-300').textContent).toContain('1 项')
    expect(screen.getByTestId('spec-row-300').textContent).toContain('旧包迁移')
  })

  it('默认设置：官方推荐参数只读回显（想改请另存为自己的方案）', async () => {
    await ready()
    expect(screen.getByTestId('plugin-defaults-note').textContent).toContain('官方推荐参数，不可改')
    expect(screen.getByTestId('plugin-paramform-defaults-note')).toBeTruthy()
    const input = screen.getByTestId('plugin-paramform-input-shortSideMin') as HTMLInputElement
    expect(input.value).toBe('1600')
    expect(input.disabled).toBe(true)
  })

  it('不放任务进度 / 历史结果：只给「结果在哪看」的出口', async () => {
    await ready()
    expect(screen.getByTestId('plugin-detail-where')).toBeTruthy()
    // 页面上没有「结果表 / 失败列表 / 进度卡」这类区域（出口只有链接）
    expect(screen.queryByTestId('compliance-job-detail')).toBeNull()
    expect(screen.queryByTestId('compliance-jobs-panel')).toBeNull()
    expect(screen.queryByTestId('judge-progress')).toBeNull()
  })
})

describe('插件详情页：outputs 差异（判定类 vs 处理类）', () => {
  it('判定类：有复核说明，出口指向处理记录（KI-35 P2 命名表）', async () => {
    await ready(['verdict'])
    expect(screen.getByTestId('plugin-detail-outputs').textContent).toBe('检查')
    expect(screen.getByTestId('plugin-review-note').textContent).toContain('复核')
    expect(screen.getByTestId('plugin-review-note').textContent).toContain('处理记录')
    expect(screen.queryByTestId('plugin-process-note')).toBeNull()
  })

  it('处理类：不提审核列', async () => {
    await ready(['artifact'])
    expect(screen.getByTestId('plugin-detail-outputs').textContent).toBe('处理')
    expect(screen.getByTestId('plugin-process-note')).toBeTruthy()
    expect(screen.queryByTestId('plugin-review-note')).toBeNull()
  })
})

describe('插件详情页：未安装（灰态但看得见声明）', () => {
  it('schema 可见但控件灰态；规格仍可见；版本区给出未安装 + 安装入口', async () => {
    apiMock.compliancePacks.mockImplementation(async () =>
      packsView({
        plugins: [
          packOf({
            currentVersion: null,
            versions: [],
            runtime: { engine: 'plugin', available: false, reason: 'plugin_pack_missing', note: '未找到包目录' },
          }),
        ],
      }),
    )
    apiMock.specs.mockImplementation(async () => ({
      total: 1,
      items: [
        specOf({
          pluginStatus: {
            installed: false,
            version: null,
            versionMismatch: false,
            paramsSchemaDeclared: true,
            rulesAvailable: [],
            note: `${PLUGIN_ID} 未安装：规格保留可见，装好后再执行`,
          },
        }),
      ],
    }))
    apiMock.compliancePluginPackDetail.mockImplementation(async () => ({
      ...detailOf(['verdict']),
      id: PLUGIN_ID,
      error: 'plugin_pack_missing',
    }))
    apiMock.pluginParamsSchema.mockImplementation(async () => ({
      pluginId: PLUGIN_ID,
      version: '1.3.0',
      pluginHash: 'a'.repeat(64),
      declared: true,
      schemaFile: 'resources/params.schema.json',
      schema: SCHEMA,
    }))
    renderDetail()
    await screen.findByTestId('plugin-section-params')

    // 参数：灰态 + 「装好后可用」
    expect(screen.getByTestId('plugin-paramform-disabled-note').textContent).toContain('装好后可用')
    // KI-35 P2：未安装态给「去设置装包」的直连入口（不只一行灰态说明）
    const installLink = screen.getByTestId('plugin-params-install-link') as HTMLAnchorElement
    expect(installLink.getAttribute('href')).toBe('/settings?drawer=packs')
    expect((screen.getByTestId('plugin-paramform-input-shortSideMin') as HTMLInputElement).disabled).toBe(true)
    // 规格：照旧可见（卸载/未装不删用户配置）
    expect(screen.getByTestId('spec-name-300')).toBeTruthy()
    expect(screen.getByTestId('spec-note-300').textContent).toContain('未安装')
    // 版本区：未安装说明 + 安装入口在
    expect(screen.getByTestId('plugin-not-installed').textContent).toContain('未安装任何版本')
    expect(screen.getByTestId('plugin-install-file')).toBeTruthy()
    // 装载态徽标
    expect(screen.getByTestId('plugin-detail-runtime').textContent).toContain('缺包')
  })
})

describe('插件详情页：内置包（账本里一行，builtin=true）', () => {
  it('内置包身份来自 packs.plugins[]：显示内置/可用/版本，卸载灰置（随产品分发）', async () => {
    // IP-K16 起插件事实只有一个来源：账本 packs.plugins[]（内置包也是一行，builtin: true）。
    // 旧的 `packs.builtin[].plugin` 身份随 YAML 行业包一起删掉了，不再有第二条身份来源。
    apiMock.compliancePacks.mockImplementation(async () => packsView())
    apiMock.pluginParamsSchema.mockImplementation(async () => ({
      pluginId: PLUGIN_ID,
      version: '1.3.0',
      pluginHash: 'a'.repeat(64),
      declared: true,
      schemaFile: 'resources/params.schema.json',
      schema: SCHEMA,
    }))
    apiMock.specs.mockImplementation(async () => ({ total: 1, items: [specOf()] }))
    apiMock.compliancePluginPackDetail.mockImplementation(async () => detailOf(['verdict']))
    renderDetail()
    await screen.findByTestId('plugin-section-versions')

    expect(screen.getByTestId('plugin-detail-builtin').textContent).toBe('内置')
    expect(screen.getByTestId('plugin-detail-runtime').textContent).toContain('可用')
    // 当前版本来自账本（不是旧内置身份对象）
    expect(screen.getByTestId('plugin-cap-version').textContent).toContain('1.3.0')
    // 有版本行 → 不是「未安装」
    expect(screen.queryByTestId('plugin-not-installed')).toBeNull()
    expect(screen.getByTestId(`plugin-version-current-${PLUGIN_ID}-1.3.0`)).toBeTruthy()
    // 内置包随产品分发：卸载按钮在但灰置，理由写在 title 上
    const del = screen.getByTestId(`plugin-uninstall-${PLUGIN_ID}-1.3.0`) as HTMLButtonElement
    expect(del.disabled).toBe(true)
    expect(del.title).toContain('不可卸载')
  })
})

describe('插件详情页：新建方案（同一份 ParamForm）', () => {
  it('填名 + 勾规则 + 只提交改过的参数 → POST /api/specs', async () => {
    await ready()
    fireEvent.click(screen.getByTestId('plugin-spec-new'))
    await screen.findByTestId('spec-editor')

    fireEvent.change(screen.getByTestId('spec-name'), { target: { value: '我的 800px webp' } })
    fireEvent.click(screen.getByTestId('spec-rule-R2_white_bg'))
    // 参数：只改 shortSideMin（whiteMaxDev 保持默认 → 不进 diff）
    fireEvent.change(screen.getByTestId('spec-paramform-input-shortSideMin'), { target: { value: '800' } })
    fireEvent.click(screen.getByTestId('spec-editor-save'))

    await waitFor(() => expect(apiMock.specCreate).toHaveBeenCalledTimes(1))
    const req = apiMock.specCreate.mock.calls[0][0] as Record<string, unknown>
    expect(req.pluginId).toBe(PLUGIN_ID)
    expect(req.name).toBe('我的 800px webp')
    expect(req.rules).toEqual(['R2_white_bg'])
    expect(req.paramsDiff).toEqual({ shortSideMin: 800 })
    // 新规格出现在列表里
    await screen.findByTestId('spec-row-400')
  })

  it('方案名为空 → 就地报错，不调接口', async () => {
    await ready()
    fireEvent.click(screen.getByTestId('plugin-spec-new'))
    await screen.findByTestId('spec-editor')
    fireEvent.click(screen.getByTestId('spec-editor-save'))
    expect(screen.getByTestId('spec-editor-error').textContent).toContain('方案名不能为空')
    expect(apiMock.specCreate).not.toHaveBeenCalled()
  })

  it('编辑方案：PATCH 只带改动的键，历史阈值只读回显不丢', async () => {
    await ready()
    fireEvent.click(screen.getByTestId('spec-edit-300'))
    await screen.findByTestId('spec-editor')
    // 旧包迁移过来的阈值原文必须在（不许静默丢）
    expect(screen.getByTestId('spec-legacy-config').textContent).toContain('R1_size')
    fireEvent.change(screen.getByTestId('spec-paramform-input-whiteMaxDev'), { target: { value: '4' } })
    fireEvent.click(screen.getByTestId('spec-editor-save'))
    await waitFor(() => expect(apiMock.specUpdate).toHaveBeenCalledTimes(1))
    const [id, req] = apiMock.specUpdate.mock.calls[0] as [number, Record<string, unknown>]
    expect(id).toBe(300)
    expect(req.name).toBe('淘宝主图')
    expect(req.paramsDiff).toEqual({ shortSideMin: 1800, whiteMaxDev: 4 })
  })
})

describe('插件详情页：页头两个固定动作', () => {
  it('设为默认 → 行内出现「默认」徽标（界面偏好，不动插件默认值）', async () => {
    await ready()
    fireEvent.click(screen.getByTestId('spec-default-set-301'))
    await screen.findByTestId('spec-default-301')
    expect(screen.queryByTestId('spec-default-300')).toBeNull()
  })

  it('先试一张：试跑后不跳走，单张结果就地显示（通过/未通过/待人工看 + 原因 + 把握度）', async () => {
    await ready()
    fireEvent.click(screen.getByTestId('plugin-try-one'))
    await waitFor(() => expect(apiMock.specTry).toHaveBeenCalledWith(300))
    const panel = await screen.findByTestId('plugin-try-result')
    // 留在本页（批量结果才去处理记录）
    expect(screen.getByTestId('loc').textContent).toBe(`/plugins/${PLUGIN_ID}`)
    expect(panel.textContent).toContain('通过')
    expect(panel.textContent).toContain('未通过')
    expect(panel.textContent).toContain('待人工看')
    expect(panel.textContent).toContain('R2_white_bg')
    expect(panel.textContent).toContain('把握度')
    expect(screen.getByTestId('plugin-try-row-1').textContent).toContain('未通过')
    // 整批入口带这一次的 runId
    expect(screen.getByTestId('plugin-try-open-record').getAttribute('href')).toBe('/tasks?task=42')
  })

  it('先试一张在跑时先显示「正在试」，跑完自动换成结果（只轮询这一次，不做进度条）', async () => {
    await ready()
    let n = 0
    apiMock.complianceJob.mockImplementation(async () => {
      n += 1
      return {
        run: { runId: 42, status: n === 1 ? 'running' : 'done', statusLabel: '' },
        taskTotal: 0,
        limit: 1,
        offset: 0,
        tasks: [],
      }
    })
    fireEvent.click(screen.getByTestId('plugin-try-one'))
    await screen.findByTestId('plugin-try-running')
    await waitFor(
      () => expect(screen.getByTestId('plugin-try-result').textContent).toContain('通过'),
      { timeout: 4000 },
    )
  })

  it('处理类先试一张：显示处理后的图（缩略图）与去向说明', async () => {
    apiMock.compliancePacks.mockImplementation(async () => packsView())
    apiMock.pluginParamsSchema.mockImplementation(async () => ({
      pluginId: PLUGIN_ID,
      version: '1.3.0',
      pluginHash: 'a'.repeat(64),
      declared: true,
      schemaFile: 'resources/params.schema.json',
      schema: SCHEMA,
    }))
    apiMock.specs.mockImplementation(async () => ({ total: 1, items: [specOf()] }))
    apiMock.compliancePluginPackDetail.mockImplementation(async () => detailOf(['artifact']))
    apiMock.complianceResults.mockImplementation(async () =>
      resultsViewOf(['artifact'], [
        resultRowOf({ assetId: 9, ruleId: '', judgementScope: null, verdict: null }),
      ]),
    )
    renderDetail()
    await screen.findByTestId('plugin-section-specs')
    fireEvent.click(screen.getByTestId('plugin-try-one'))
    const panel = await screen.findByTestId('plugin-try-result')
    const img = screen.getByTestId('plugin-try-thumb') as HTMLImageElement
    expect(img.getAttribute('src')).toBe('/api/media/thumb/9?size=1024')
    expect(panel.textContent).toContain('已执行')
    expect(panel.textContent).toContain('新版本')
    // 处理类不出现结论词（处理没有通过与未通过）
    expect(panel.textContent).not.toContain('待人工看')
  })

  it('判定引擎当前跑的是别的插件时也能试跑（KI-31 按规格自己的插件开包，不再有 spec_plugin_mismatch）', async () => {
    apiMock.compliancePacks.mockImplementation(async () =>
      packsView({
        // 判定状态面上报的是另一个插件：KI-31 之后这不再是失败前置条件
        pluginState: {
          id: 'com.soujpg.pack.ecommerce_ui',
          available: true,
          version: '9.9.9',
          capabilities: [],
          deterministic: false,
        },
      }),
    )
    apiMock.pluginParamsSchema.mockImplementation(async () => ({
      pluginId: PLUGIN_ID,
      version: '1.3.0',
      pluginHash: 'a'.repeat(64),
      declared: true,
      schemaFile: 'resources/params.schema.json',
      schema: SCHEMA,
    }))
    apiMock.specs.mockImplementation(async () => ({ total: 1, items: [specOf()] }))
    apiMock.compliancePluginPackDetail.mockImplementation(async () => detailOf(['verdict']))
    renderDetail()
    await screen.findByTestId('plugin-section-specs')
    const btn = screen.getByTestId('plugin-try-one') as HTMLButtonElement
    expect(btn.disabled).toBe(false)
    // 旧实现会在这里灰掉按钮并给一行「判定引擎当前跑的是 …」的说明；那条路已经不存在了
    expect(screen.queryByTestId('plugin-try-blocked-note')).toBeNull()
    fireEvent.click(btn)
    await waitFor(() => expect(apiMock.specTry).toHaveBeenCalledWith(300))
  })

  it('规格编辑浮层挂在 document.body 下（不在 data-overlay-base 里，否则被自己的 inert 锁死）', async () => {
    await ready()
    fireEvent.click(screen.getByTestId('plugin-spec-new'))
    const panel = await screen.findByTestId('spec-editor')
    expect(panel.closest('[data-overlay-base]')).toBeNull()
    expect(document.querySelector('[data-testid="spec-editor-mask"]')!.parentElement).toBe(document.body)
    // 打开浮层 → 背景壳被 inert（背景不可点），但浮层自己必须在 inert 之外
    expect(document.querySelector('[data-overlay-base]')!.hasAttribute('inert')).toBe(true)
    expect(panel.closest('[inert]')).toBeNull()
  })

  it('没有可用方案时「先试一张」禁用（不让人撞一个必失败的请求）', async () => {
    apiMock.compliancePacks.mockImplementation(async () => packsView())
    apiMock.pluginParamsSchema.mockImplementation(async () => ({ declared: false, schema: null, pluginId: PLUGIN_ID, version: '1.3.0', pluginHash: 'x', schemaFile: null }))
    apiMock.specs.mockImplementation(async () => ({ total: 0, items: [] }))
    apiMock.compliancePluginPackDetail.mockImplementation(async () => detailOf(['verdict']))
    renderDetail()
    await screen.findByTestId('plugin-specs-empty')
    expect((screen.getByTestId('plugin-try-one') as HTMLButtonElement).disabled).toBe(true)
  })
})

describe('插件详情页：删除方案 / 安装与版本（唯一一处）', () => {
  it('删除方案要确认，确认后只删配置行', async () => {
    await ready()
    fireEvent.click(screen.getByTestId('spec-delete-301'))
    await screen.findByText(/删除方案「亚马逊主图」？/)
    fireEvent.click(screen.getByTestId('confirm-ok'))
    await waitFor(() => expect(apiMock.specDelete).toHaveBeenCalledWith(301))
  })

  it('第三方包：回滚 / 切当前 / 卸载都在这页', async () => {
    apiMock.compliancePacks.mockImplementation(async () =>
      packsView({ plugins: [packOf({ builtin: false, staleCurrent: '1.1.0' })] }),
    )
    apiMock.pluginParamsSchema.mockImplementation(async () => ({ declared: false, schema: null, pluginId: PLUGIN_ID, version: '1.3.0', pluginHash: 'x', schemaFile: null }))
    apiMock.specs.mockImplementation(async () => ({ total: 0, items: [] }))
    apiMock.compliancePluginPackDetail.mockImplementation(async () => detailOf(['verdict']))
    renderDetail()
    await screen.findByTestId('plugin-section-versions')

    // 指针失效要显式说（「看起来有包、判定却起不来」的典型来源）
    expect(screen.getByTestId('plugin-stale').textContent).toContain('1.1.0')
    fireEvent.click(screen.getByTestId('plugin-rollback'))
    await waitFor(() => expect(apiMock.compliancePackRollback).toHaveBeenCalled())

    fireEvent.click(await screen.findByTestId(`plugin-setcurrent-${PLUGIN_ID}-1.2.0`))
    await waitFor(() => expect(apiMock.compliancePackActivateVersion).toHaveBeenCalledWith(PLUGIN_ID, '1.2.0'))

    fireEvent.click(screen.getByTestId(`plugin-uninstall-${PLUGIN_ID}-1.2.0`))
    await screen.findByText(/卸载 .*1\.2\.0？/)
    fireEvent.click(screen.getByTestId('confirm-ok'))
    await waitFor(() => expect(apiMock.compliancePackUninstall).toHaveBeenCalledWith(PLUGIN_ID, '1.2.0'))
  })

  it('内置包不给卸载（按钮禁用 + 说明为什么不给）', async () => {
    await ready()
    const btn = screen.getByTestId(`plugin-uninstall-${PLUGIN_ID}-1.2.0`) as HTMLButtonElement
    expect(btn.disabled).toBe(true)
    expect(btn.getAttribute('title')).toContain('内置包')
  })

  it('KI35-P4：只剩最后一个版本 → 整包卸载（不带 version），确认框说清「导航会消失」', async () => {
    // 走查缺口：单版本（必然是 current）按版本卸会被 409 pack_version_current 拒死，
    // UI 又没有别的整包卸载入口 → 导航能力项永远减不掉。修法：最后一个版本走
    // version-less DELETE（目录全删、账本留审计、staleCurrent → 导航减项）。
    apiMock.compliancePacks.mockImplementation(async () =>
      packsView({
        plugins: [
          packOf({
            builtin: false,
            versions: [
              {
                version: '1.3.0',
                dir: `/test-fixture/plugins/${PLUGIN_ID}/1.3.0`,
                present: true,
                manifestHash: 'a'.repeat(64),
                apiVersion: '1.1',
                deterministic: false,
                signatureStatus: 'declared',
                installedAt: 200,
                current: true,
              },
            ],
          }),
        ],
      }),
    )
    apiMock.compliancePluginPackDetail.mockImplementation(async () => detailOf(['artifact']))
    apiMock.specs.mockImplementation(async () => ({ total: 0, items: [] }))
    renderDetail()
    await screen.findByTestId('plugin-section-versions')

    fireEvent.click(screen.getByTestId(`plugin-uninstall-${PLUGIN_ID}-1.3.0`))
    // 确认框改口：最后一个版本 = 整包卸载，且说清后果（能力从导航消失）
    expect((await screen.findByText(/这是最后一个已装版本/)).textContent).toContain('整包卸载')
    fireEvent.click(screen.getByTestId('confirm-ok'))
    await waitFor(() => expect(apiMock.compliancePackUninstall).toHaveBeenCalledWith(PLUGIN_ID, undefined))
  })
})
