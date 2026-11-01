// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter, Route, Routes, useLocation } from 'react-router-dom'
import { ToastProvider } from '../src/lib/toast'
import { ExportDialog } from '../src/components/ExportDialog'
import { ExportRedirect, NAV_ITEMS } from '../src/App'
import { stashExportFilterScope } from '../src/lib/export'
import type { ExportFilterPayload } from '../src/lib/api'

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
 * 卡 t_4bc7fa24（W 布局裁决：导出动作化）—— 单测钉死四件事：
 *
 *  1. **导航撤项**：一级导航不再有「导出」（入口只剩三处：图库/搜索选择集按钮 + 命令面板动作），
 *     `NAV_ITEMS` 长度按撤项后登记（少 1 项不是靠肉眼确认，靠断言）。
 *  2. **老深链不 404**：`/export` 一律重定向到 `/?dialog=export`（图库 + 全局 Dialog）。
 *  3. **范围二选一，且没有第二套资产筛选**：Dialog 只剩「当前选择集 N 张 / 当前筛选结果 M 张」
 *     + 内容 / 打包 / 目标路径 / manifest；导出页那套「目录/标签/色标/星级」控件整块不许复活
 *     （控件结构断言：radio 组只有 export-scope / content / mode，checkbox 只有 manifest 一个）。
 *  4. **M 与载荷同源（DT4 教训）**：界面上的 M 必须来自 `api.assets` 用**导出将要下发的那份
 *     filter** 回读的 total；`exportStart` 只能收到 assetIds / filter 之一，不许两者同时出现。
 *
 * 纯前端单测；真实点击链路（图库选 5 张 → E → 切换范围 → 导出落盘 + tasks 导出记录）见
 * reports/t_4bc7fa24-export-dialog-only.md 的 CDP 取证。
 */

const apiStub = vi.hoisted(() => ({
  appInfo: vi.fn(),
  assets: vi.fn(),
  exportStart: vi.fn(),
  exportStatus: vi.fn(),
  exportCancel: vi.fn(),
  capabilities: vi.fn(),
}))

vi.mock('../src/lib/api', () => ({
  api: apiStub,
  formatSize: (n: number) => `${n} B`,
  ApiError: class ApiError extends Error {
    code?: string
    status = 400
    constructor(msg: string, status = 400, code?: string) {
      super(msg)
      this.code = code
      this.status = status
    }
  },
}))

const FILTER: ExportFilterPayload = { tag: '南瓜', dirPrefix: '/test-fixture/in', broken: 1 }

function renderDialog(path: string) {
  return render(
    <ToastProvider>
      <MemoryRouter initialEntries={[path]}>
        <ExportDialog />
      </MemoryRouter>
    </ToastProvider>,
  )
}

/** 对话框里的全部表单控件（范围 / 内容 / 打包 / manifest —— 多一个都算「第二套筛选」复活） */
function controls() {
  const dlg = screen.getByTestId('export-dialog')
  const inputs = Array.from(dlg.querySelectorAll('input')) as HTMLInputElement[]
  return {
    radios: inputs.filter((i) => i.type === 'radio'),
    checkboxes: inputs.filter((i) => i.type === 'checkbox'),
    textInputs: inputs.filter((i) => i.type === 'text' || i.type === '' || i.type === 'search'),
    selects: Array.from(dlg.querySelectorAll('select')) as HTMLSelectElement[],
    radioGroups: [...new Set(inputs.filter((i) => i.type === 'radio').map((i) => i.name))].sort(),
  }
}

beforeEach(() => {
  sessionStorage.clear()
  apiStub.appInfo.mockResolvedValue({ dataDir: '/data' })
  apiStub.assets.mockResolvedValue({ items: [], total: 7 })
  apiStub.exportStatus.mockResolvedValue({ running: false, progress: null, report: null, error: null })
  apiStub.exportStart.mockResolvedValue({ ok: true, total: 3 })
  apiStub.exportCancel.mockResolvedValue({ ok: true })
  apiStub.capabilities.mockResolvedValue({ features: {} })
})

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
})

describe('导航与深链：导出不再是一级入口，老链接不 404', () => {
  it('NAV_ITEMS 撤掉「导出」与第二档能力 tab（KI-35 P1：能力区改由已装包驱动）', () => {
    // KI-35 P1：静态导航只剩区域与内置功能；检查/处理动词 tab 与 /compliance 路由都不在
    expect(NAV_ITEMS.map((n) => n.to)).toEqual(['/', '/search', '/tag-tasks', '/settings'])
    expect(NAV_ITEMS.map((n) => n.label)).not.toContain('导出')
    expect(NAV_ITEMS.map((n) => n.label)).not.toContain('检查')
    expect(NAV_ITEMS.map((n) => n.label)).not.toContain('处理')
    // 插件/方案目录只在设置里（导航里不出现这类词）
    for (const label of NAV_ITEMS.map((n) => n.label)) {
      expect(label).not.toMatch(/插件|行业包|方案|任务号/)
    }
    expect(NAV_ITEMS.length).toBe(4)
  })

  it('/export 重定向到 /?dialog=export（replace，不留历史），图库照常渲染', async () => {
    function Probe() {
      const loc = useLocation()
      return (
        <div data-testid="probe" data-pathname={loc.pathname} data-search={loc.search}>
          gallery
        </div>
      )
    }
    render(
      <ToastProvider>
        <MemoryRouter initialEntries={['/export']}>
          <Routes>
            <Route path="/export" element={<ExportRedirect />} />
            <Route path="/" element={<Probe />} />
          </Routes>
        </MemoryRouter>
      </ToastProvider>,
    )
    const probe = await screen.findByTestId('probe')
    expect(probe.getAttribute('data-pathname')).toBe('/')
    expect(probe.getAttribute('data-search')).toBe('?dialog=export')
  })
})

describe('范围摘要：选择集 N 张 ↔ 当前筛选结果 M 张（二选一）', () => {
  it('打开时默认选中「当前选择集 3 张」，M 用导出同一份 filter 回读（7 张）', async () => {
    stashExportFilterScope({ label: '图库当前筛选', filter: FILTER })
    renderDialog('/?dialog=export&ids=1,2,3')

    const selRadio = screen.getByTestId('export-scope-selection') as HTMLInputElement
    const filterRadio = screen.getByTestId('export-scope-filter') as HTMLInputElement
    expect(selRadio.checked).toBe(true)
    expect(filterRadio.checked).toBe(false)
    expect(screen.getByTestId('export-scope-selection-label').textContent).toContain('3')
    expect(screen.getByTestId('export-scope-selection-label').textContent).toContain('张')

    // M = /api/library/assets 同条件 total；条件必须是导出真正下发的那份 filter（DT4：不得另算一套）
    await waitFor(() => expect(screen.getByTestId('export-scope-filter-total').textContent).toBe('7'))
    expect(apiStub.assets).toHaveBeenCalledWith({ ...FILTER, limit: 1 })
    expect(screen.getByTestId('export-scope-filter-label').textContent).toContain('图库当前筛选')
  })

  it('切换范围是单选（不会两个都选中），且标题随范围变化', async () => {
    stashExportFilterScope({ label: '图库当前筛选', filter: FILTER })
    renderDialog('/?dialog=export&ids=1,2,3')
    const selRadio = screen.getByTestId('export-scope-selection') as HTMLInputElement
    const filterRadio = screen.getByTestId('export-scope-filter') as HTMLInputElement

    fireEvent.click(filterRadio)
    expect(filterRadio.checked).toBe(true)
    expect(selRadio.checked).toBe(false)
    const checkedRadios = controls().radios.filter((r) => r.name === 'export-scope' && r.checked)
    expect(checkedRadios).toHaveLength(1)

    fireEvent.click(selRadio)
    expect(selRadio.checked).toBe(true)
    expect(filterRadio.checked).toBe(false)
  })

  it('没有寄存筛选上下文（深链直开）→ 「当前筛选结果」不出现，不虚造范围', () => {
    renderDialog('/?dialog=export&ids=1,2')
    expect(screen.queryByTestId('export-scope-filter')).toBeNull()
    expect(screen.getByTestId('export-scope-selection-label').textContent).toContain('2')
  })

  it('只有筛选上下文、没有选择集（旧 /export 深链落点）→ 默认落在「当前筛选结果」', async () => {
    stashExportFilterScope({ label: '图库当前筛选', filter: {} })
    renderDialog('/?dialog=export')
    const filterRadio = screen.getByTestId('export-scope-filter') as HTMLInputElement
    expect(filterRadio.checked).toBe(true)
    await waitFor(() => expect(screen.getByTestId('export-scope-filter-total').textContent).toBe('7'))
  })

  it('筛选命中 0 张 → 「开始导出」灰置（0 张导出没有意义，不是静默空跑）', async () => {
    apiStub.assets.mockResolvedValue({ items: [], total: 0 })
    stashExportFilterScope({ label: '图库当前筛选', filter: FILTER })
    renderDialog('/?dialog=export&ids=1')
    await waitFor(() => expect(screen.getByTestId('export-scope-filter-total').textContent).toBe('0'))
    fireEvent.click(screen.getByTestId('export-scope-filter'))
    const submit = screen.getByRole('button', { name: /开始导出/ }) as HTMLButtonElement
    expect(submit.disabled).toBe(true)
  })
})

describe('表单里没有第二套资产筛选（导出页那套控件不许复活）', () => {
  it('控件结构：radio 组只有 export-scope / content / mode，checkbox 只有 manifest，没有任何 select', () => {
    stashExportFilterScope({ label: '图库当前筛选', filter: FILTER })
    renderDialog('/?dialog=export&ids=1,2,3')
    const c = controls()
    expect(c.radioGroups).toEqual(['content', 'export-scope', 'mode'])
    expect(c.checkboxes).toHaveLength(1) // 只有「写 manifest.csv」
    expect(c.selects).toHaveLength(0) // 旧导出页的类型/星级/色标全是 select
    // 文本输入只剩「目标路径」一个（旧导出页还有 扩展名 / 标签 / 目录前缀 三个）
    expect(c.textInputs).toHaveLength(1)
  })

  it('界面上不出现 标签 / 色标 / 星级 / 只看未打标 / 扩展名 这些筛选措辞', () => {
    stashExportFilterScope({ label: '图库当前筛选', filter: FILTER })
    renderDialog('/?dialog=export&ids=1,2,3')
    const text = screen.getByTestId('export-dialog').textContent ?? ''
    for (const word of ['色标', '星级', '只看未打标', '扩展名', '媒体类型']) {
      expect(text, `对话框里出现第二套筛选措辞：${word}`).not.toContain(word)
    }
  })
})

describe('导出载荷：assetIds / filter 严格二选一（范围决定，不双发）', () => {
  async function submit() {
    fireEvent.submit(screen.getByTestId('export-form'))
  }

  it('选择集范围 → assetIds=[...]，不带 filter', async () => {
    stashExportFilterScope({ label: '图库当前筛选', filter: FILTER })
    renderDialog('/?dialog=export&ids=1,2,3')
    await waitFor(() => expect(screen.getByTestId('export-scope-filter-total').textContent).toBe('7'))
    await submit()
    await waitFor(() => expect(apiStub.exportStart).toHaveBeenCalledTimes(1))
    const req = apiStub.exportStart.mock.calls[0][0] as Record<string, unknown>
    expect(req.assetIds).toEqual([1, 2, 3])
    expect('filter' in req).toBe(false)
    expect(req.content).toBe('original')
    expect(req.manifest).toBe(true)
  })

  it('筛选范围 → filter 原样下发（与界面 M 同一对象内容），不带 assetIds', async () => {
    stashExportFilterScope({ label: '图库当前筛选', filter: FILTER })
    renderDialog('/?dialog=export&ids=1,2,3')
    await waitFor(() => expect(screen.getByTestId('export-scope-filter-total').textContent).toBe('7'))
    fireEvent.click(screen.getByTestId('export-scope-filter'))
    await submit()
    await waitFor(() => expect(apiStub.exportStart).toHaveBeenCalledTimes(1))
    const req = apiStub.exportStart.mock.calls[0][0] as Record<string, unknown>
    // 计数与导出必须同一份条件：资产端点收到的 {..filter, limit:1} 去掉 limit 后等于这里
    const counted = apiStub.assets.mock.calls[0][0] as Record<string, unknown>
    const { limit: _limit, ...countedFilter } = counted
    expect(req.filter).toEqual(countedFilter)
    expect(req.filter).toEqual(FILTER)
    expect('assetIds' in req).toBe(false)
  })

  it('脏 ids（?ids=abc）+ 无筛选上下文 → 标题 0 张、提交被拦（不误报「导出成功」）', async () => {
    renderDialog('/?dialog=export&ids=abc')
    const btn = screen.getByRole('button', { name: /开始导出/ }) as HTMLButtonElement
    expect(btn.disabled).toBe(true)
    fireEvent.submit(screen.getByTestId('export-form'))
    expect(apiStub.exportStart).not.toHaveBeenCalled()
  })
})
