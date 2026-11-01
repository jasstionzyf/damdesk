// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter, Route, Routes, useLocation } from 'react-router-dom'
import { PackManager } from '../src/components/PackManager'
import { ToastProvider } from '../src/lib/toast'
import { ConfirmProvider } from '../src/lib/confirm'

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
 * IP-K09 + N4 前端验收（jsdom + RTL；只有 `api` 是桩，组件树 / toast / 文件读取都是真的）。
 *
 * N4（卡 t_d927e0d4）把插件面收敛成一页：**本文件**守「行业包管理这一页只做清单 +
 * 首次安装 + 导航」，详情页（能力说明 / 我的规格 / 参数默认值 / 切版本·回滚·卸载）由
 * `pluginDetail.dom.test.tsx` 守 —— 两边合起来才是「不留两套插件页」。
 *
 *  T1 清单按包展示：当前版本 / 是否内置 / 装载态；三种状态都要有话说
 *     （未安装、指针失效 stale-current、包损坏）；
 *  T2 行内只留「详情」：版本表 / 回滚 / 卸载 / manifest 预览**不在本页**（点详情形 /plugins/<id>）；
 *  T3 装 `.dampack` 走文件选择 → archiveBase64；回执把签名策略 / 验签结果 / 能力声明 /
 *     摘要核对留在界面上（验签失败要红着说，不能只当成功注脚），装完去该插件详情页；
 *  T4 装载态只讲后端能静态判定的事（manifest 就绪 / 缺包 / 包损坏），
 *     并明说 running·冷却·崩溃 属判定时刻运行态、不在静态快照内。
 */

const { apiMock } = vi.hoisted(() => ({
  apiMock: {
    compliancePacks: vi.fn(),
    complianceActivate: vi.fn(),
    complianceImport: vi.fn(),
    compliancePackPreview: vi.fn(),
    compliancePackInstall: vi.fn(),
  },
}))

vi.mock('../src/lib/api', () => ({ api: apiMock }))

const ACTIVE = { source: 'builtin' as const, id: 'ecommerce', activatedAt: 0 }

function ver(over: Record<string, unknown>) {
  return {
    dir: '/test-fixture/plugins/x/1.0.0',
    present: true,
    manifestHash: 'a'.repeat(64),
    apiVersion: '1.1',
    deterministic: true,
    signatureStatus: 'verified',
    installedAt: 100,
    current: false,
    registered: true,
    ...over,
  }
}

/** 内置包：两个版本并存，当前 1.3.0 */
const BUILTIN_PACK = {
  id: 'ecommerce',
  builtin: true,
  currentVersion: '1.3.0',
  staleCurrent: null,
  versions: [
    ver({ version: '1.3.0', dir: '/test-fixture/plugins/ecommerce/1.3.0', current: true, installedAt: 200 }),
    ver({ version: '1.2.0', dir: '/test-fixture/plugins/ecommerce/1.2.0', installedAt: 100 }),
  ],
  runtime: { engine: 'plugin', available: true, id: 'ecommerce', version: '1.3.0', signatureStatus: 'verified' },
}

/** 第三方包：当前指针指向已删目录（staleCurrent=0.8.0），另两个版本目录都在 */
const THIRD_PARTY_PACK = {
  id: 'acme',
  builtin: false,
  currentVersion: null,
  staleCurrent: '0.8.0',
  versions: [
    ver({ version: '1.0.1', dir: '/test-fixture/plugins/acme/1.0.1', signatureStatus: 'declared', installedAt: 300 }),
    ver({
      version: '0.8.0',
      dir: '/test-fixture/plugins/acme/0.8.0',
      present: false,
      manifestHash: null,
      apiVersion: null,
      deterministic: null,
      signatureStatus: null,
      installedAt: 200,
    }),
  ],
  runtime: {
    engine: 'plugin',
    available: false,
    reason: 'plugin_pack_missing',
    note: '未找到行业包目录（acme）：装包后再切 plugin 引擎',
  },
}

/** 坏包：一个版本都没装上（从未安装），manifest 解析失败 */
const BROKEN_PACK = {
  id: 'broken',
  builtin: false,
  currentVersion: null,
  staleCurrent: null,
  versions: [],
  runtime: {
    engine: 'plugin',
    available: false,
    reason: 'plugin_manifest_invalid',
    note: 'manifest.json: missing field `api_version`',
  },
}

function packsView(over: Record<string, unknown> = {}) {
  return {
    activePack: ACTIVE,
    builtin: [
      { id: 'ecommerce', source: 'builtin', rules: 6, taxonomyFields: 6, steps: 3, error: null },
    ],
    imported: [],
    plugins: [BUILTIN_PACK, THIRD_PARTY_PACK, BROKEN_PACK],
    signaturePolicy: { required: true, trustedKeys: 2 },
    hostCapabilities: ['read_image', 'crop', 'upscale'],
    ...over,
  }
}

function installPayload(over: Record<string, unknown> = {}) {
  return {
    ok: true,
    id: 'acme',
    version: '1.1.0',
    dir: '/test-fixture/plugins/acme/1.1.0',
    manifestHash: 'c'.repeat(64),
    apiVersion: '1.1',
    deterministic: true,
    capabilities: ['read_image', 'crop'],
    signatureStatus: 'verified',
    signaturePolicy: { required: true, trustedKeys: 2 },
    digestsChecked: true,
    reused: false,
    activated: false,
    previousVersion: null,
    currentVersion: null,
    filename: 'acme-1.1.0.dampack',
    source: 'upload',
    ...over,
  }
}

function LocationSpy() {
  const loc = useLocation()
  return <div data-testid="loc">{`${loc.pathname}${loc.search}`}</div>
}

beforeEach(() => {
  apiMock.compliancePacks.mockImplementation(async () => packsView())
  apiMock.compliancePackInstall.mockImplementation(async () => installPayload())
})

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
})

function renderManager() {
  return render(
    <MemoryRouter initialEntries={['/settings?drawer=packs']}>
      <ToastProvider>
        <ConfirmProvider>
          <Routes>
            <Route path="/settings" element={<PackManager />} />
            <Route path="/processing/:pluginId" element={<div data-testid="plugin-page">插件详情</div>} />
          </Routes>
          <LocationSpy />
        </ConfirmProvider>
      </ToastProvider>
    </MemoryRouter>,
  )
}

/** 等插件包清单出第一屏 */
async function ready() {
  renderManager()
  await screen.findByTestId('pack-manager-plugins')
  await screen.findByTestId('plugin-pack-ecommerce')
}

/** 造一个假 `.dampack` 文件（内容只需能被读成 base64；服务端校验不在前端测） */
function fakeArchive(name = 'acme-1.1.0.dampack') {
  const bytes = new Uint8Array([0x50, 0x4b, 0x03, 0x04, 0x00, 0x01, 0x02, 0x03])
  return { file: new File([bytes], name), bytes }
}

async function pickAndWait(name = 'acme-1.1.0.dampack') {
  const { file, bytes } = fakeArchive(name)
  const input = screen.getByTestId('plugin-install-file') as HTMLInputElement
  fireEvent.change(input, { target: { files: [file] } })
  // 选中后在界面上就能看见文件名（读文件是异步的）
  await waitFor(() => expect(screen.getByTestId('plugin-install-picked').textContent).toContain(name))
  return bytes
}

// ---------------------------------------------------------------- T1 清单内容

describe('T1 清单按包展示当前版本/是否内置/装载态', () => {
  it('内置包：当前版本 + 内置标记 + 可用', async () => {
    await ready()
    expect(screen.getByTestId('plugin-current-ecommerce').textContent).toBe('1.3.0')
    expect(screen.getByTestId('plugin-builtin-ecommerce').textContent).toBe('内置')
    expect(screen.getByTestId('plugin-runtime-ecommerce').textContent).toContain('可用')
    expect(screen.getByTestId('plugin-builtin-acme').textContent).toBe('第三方')
  })

  it('三种状态都有出口：指针失效（stale-current）/ 未安装 / 装载态缺包与坏包', async () => {
    await ready()
    // stale-current：指针指向已删目录 → 显式告警，不是静默
    expect(screen.getByTestId('plugin-stale-acme').textContent).toContain('0.8.0')
    // 从未装过 → 未安装出口
    expect(screen.getByTestId('plugin-not-installed-broken').textContent).toContain('未安装任何版本')

    // 装载态：可用 / 缺包 / 包损坏（只讲 manifest 能静态判定的事）
    expect(screen.getByTestId('plugin-runtime-acme').textContent).toContain('缺包')
    expect(screen.getByTestId('plugin-runtime-broken').textContent).toContain('包损坏')
    // 坏包必须带后端原文，不能只说「不可用」
    expect(screen.getByTestId('plugin-runtime-broken').getAttribute('title')).toContain('api_version')
  })

  it('一个插件都没有时说明「不装也能用内联引擎」，不留空白区', async () => {
    apiMock.compliancePacks.mockImplementation(async () => packsView({ plugins: [] }))
    renderManager()
    expect(await screen.findByTestId('pack-plugins-empty')).toBeTruthy()
  })
})

// ---------------------------------------------------------------- T2 只留导航

describe('T2 行内只留「详情」：其余动作都在详情页（不留两套插件页）', () => {
  it('本页没有版本表 / 回滚 / manifest 预览面板', async () => {
    await ready()
    expect(screen.queryByTestId('plugin-version-ecommerce-1.2.0')).toBeNull()
    expect(screen.queryByTestId('plugin-rollback-ecommerce')).toBeNull()
    expect(screen.queryByTestId('plugin-detail-panel')).toBeNull()
    expect(screen.queryByTestId('plugin-uninstall-ecommerce-1.2.0')).toBeNull()
  })

  it('点「详情」去 /processing/<id>（KI-35 P1 路由约定）', async () => {
    await ready()
    fireEvent.click(screen.getByTestId('plugin-detail-ecommerce'))
    await screen.findByTestId('plugin-page')
    expect(screen.getByTestId('loc').textContent).toBe('/processing/ecommerce')
  })
})

// ---------------------------------------------------------------- T3 安装

describe('T3 装 .dampack：文件选择 → archiveBase64 → 回执留在界面上', () => {
  it('没选文件时安装按钮不可点；选了文件才提交，且带 filename + activate 缺省', async () => {
    await ready()
    expect((screen.getByTestId('plugin-install') as HTMLButtonElement).disabled).toBe(true)

    const bytes = await pickAndWait()
    const btn = screen.getByTestId('plugin-install') as HTMLButtonElement
    expect(btn.disabled).toBe(false)
    fireEvent.click(btn)

    await waitFor(() => expect(apiMock.compliancePackInstall).toHaveBeenCalled())
    const req = apiMock.compliancePackInstall.mock.calls[0][0] as {
      archiveBase64: string
      filename: string
      activate?: boolean
    }
    expect(req.filename).toBe('acme-1.1.0.dampack')
    expect(req.activate).toBeUndefined() // 不勾「装完立刻切」= 补装旧版本不抢指针
    // base64 解码回来就是文件字节（前端只负责搬运，不解释内容）
    const decoded = Uint8Array.from(atob(req.archiveBase64), (c) => c.charCodeAt(0))
    expect(Array.from(decoded)).toEqual(Array.from(bytes))
  })

  it('回执把签名策略 / 摘要核对 / 能力声明显式留下，并标出未授权能力', async () => {
    apiMock.compliancePackInstall.mockImplementation(async () =>
      installPayload({ capabilities: ['read_image', 'gpu_vl'] }),
    )
    await ready()
    await pickAndWait()
    fireEvent.click(screen.getByTestId('plugin-install'))
    const receipt = await screen.findByTestId('plugin-install-receipt')
    expect(receipt.textContent).toContain('acme@1.1.0')
    expect(receipt.textContent).toContain('验签通过')
    expect(receipt.textContent).toContain('强制验签（2 把信任公钥）')
    expect(receipt.textContent).toContain('SHA256SUMS 逐文件通过')
    expect(receipt.textContent).toContain('read_image')
    // gpu_vl 不在本部署授权能力里 → 必须提前说清会 403
    expect(receipt.textContent).toContain('未授权的能力')
  })

  it('装完不跳走：回执留在这一屏可复查，清单里就有该包的「详情」入口', async () => {
    await ready()
    await pickAndWait()
    fireEvent.click(screen.getByTestId('plugin-install'))
    await screen.findByTestId('plugin-install-receipt')
    // 没被导航带走（回执不能被一次跳转冲掉）
    expect(screen.getByTestId('loc').textContent).toBe('/settings?drawer=packs')
    expect(screen.getByTestId('plugin-detail-acme')).toBeTruthy()
  })

  it('验签失败是错误态：回执红着说，不混成安装成功', async () => {
    apiMock.compliancePackInstall.mockImplementation(async () =>
      installPayload({ signatureStatus: 'invalid', digestsChecked: false }),
    )
    await ready()
    await pickAndWait()
    fireEvent.click(screen.getByTestId('plugin-install'))
    expect(await screen.findByText(/已装载但签名验不过/)).toBeTruthy()
    const receipt = await screen.findByTestId('plugin-install-receipt')
    expect(receipt.textContent).toContain('签名无效')
    expect(receipt.textContent).toContain('验签失败')
    expect(receipt.textContent).toContain('归档未带 SHA256SUMS')
    expect(receipt.className).toContain('rose')
  })

  it('勾「装完立刻切」→ activate:true 传下去', async () => {
    await ready()
    await pickAndWait()
    fireEvent.click(screen.getByTestId('plugin-install-activate'))
    fireEvent.click(screen.getByTestId('plugin-install'))
    await waitFor(() => expect(apiMock.compliancePackInstall).toHaveBeenCalled())
    expect((apiMock.compliancePackInstall.mock.calls[0][0] as { activate?: boolean }).activate).toBe(true)
  })
})

// ---------------------------------------------------------------- T4 策略与能力

describe('T4 签名策略 / 本部署授权能力就在装包入口旁边', () => {
  it('配了信任公钥 = 强制验签；能力集合逐条列出（不让用户对 403 无解）', async () => {
    await ready()
    expect(screen.getByTestId('pack-signature-policy').textContent).toContain('强制验签（2 把信任公钥）')
    const caps = screen.getByTestId('pack-host-caps')
    expect(caps.textContent).toContain('read_image')
    expect(caps.textContent).toContain('upscale')
  })

  it('运行态边界写在界面上：running/冷却/崩溃 属判定时刻，不以静态快照冒充', async () => {
    await ready()
    const panel = screen.getByTestId('pack-manager-plugins')
    expect(panel.textContent).toContain('冷却')
    expect(panel.textContent).toContain('崩溃')
    expect(panel.textContent).toContain('plugin_timeout')
    expect(panel.textContent).toContain('plugin_crash')
    expect(panel.textContent).toContain('plugin_unavailable')
    // 装载态 tooltip 也不冒充运行态
    expect(screen.getByTestId('plugin-runtime-ecommerce').getAttribute('title')).toContain('不在此静态快照内')
  })
})
