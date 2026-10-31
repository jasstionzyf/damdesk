// @vitest-environment jsdom
/**
 * DM-B1（task / KI-36）**手验档**：不打桩，全部走真实 `app-web`。
 *
 * 一条链路覆盖卡面验收 1 的四个位置（都取真实 DOM 文本，报告直接贴这段输出）：
 *   ① 主导航能力区 tab —— 装了**映射外**的能力（`com.soujpg.pack.upscale_demo`，仓库里没有
 *      这个 id，前端也没有任何映射表）→ 显示中文名，不是裸 id；
 *   ② 「送去处理」能力面板的行 —— 同上；
 *   ③ 能力详情页标题 —— 同上；
 *   ④ 卸载确认对话框标题 —— 同上（取消，不真卸）。
 *
 * 期望值来自**接口本身**（`/api/compliance/packs` 的 `plugins[].name/summary`），所以包改名后
 * 这条测试不用改；它断言的是「界面显示的就是接口给的那份名字」。
 *
 * 用法（容器内，先起 app-web + 装好 demo 包）：
 *   bash scripts/dm_b1_walk.sh
 */
import { expect, it } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter, Route, Routes } from 'react-router-dom'

import { CapabilityNavSection } from '../../src/App'
import { PluginDetail } from '../../src/pages/PluginDetail'
import { SendToProcessDialog } from '../../src/components/SendToProcessDialog'
import { capabilityNavItems, useCapabilityNav } from '../../src/lib/capabilityNav'
import { ConfirmProvider } from '../../src/lib/confirm'
import { ToastProvider } from '../../src/lib/toast'

// 不用 `process`（web tsconfig 只有 DOM lib）：靠 env 覆盖即可
const ENV = ((globalThis as { process?: { env?: Record<string, string | undefined> } }).process?.env ??
  {}) as Record<string, string | undefined>
const BASE = ENV.DAM_WEB_BASE ?? 'http://127.0.0.1:8791'
const DEMO_ID = ENV.DAM_DEMO_ID ?? 'com.soujpg.pack.upscale_demo'
const realFetch = globalThis.fetch.bind(globalThis)

globalThis.fetch = ((input: RequestInfo | URL, init?: RequestInit) => {
  if (typeof input === 'string' && input.startsWith('/')) return realFetch(BASE + input, init)
  return realFetch(input as RequestInfo, init)
}) as typeof fetch

const log: string[] = []
function note(line: string) {
  log.push(line)
}

/** 导航区（真实注册表 → 真实渲染） */
function Nav() {
  const { items, loaded } = useCapabilityNav()
  return <CapabilityNavSection items={items} loaded={loaded} />
}

it('真实后端：映射外的能力在 导航 / 面板 / 详情 / 卸载确认 四处都显示中文名', async () => {
  // ---------- ① 接口事实（后面所有 DOM 断言都以它为期望值） ----------
  const packs = (await (await fetch('/api/compliance/packs')).json()) as {
    plugins: { id: string; name: string | null; summary: string | null; runtime: { available: boolean } }[]
  }
  const row = packs.plugins.find((p) => p.id === DEMO_ID)
  expect(row, `${DEMO_ID} 应装在这次手验的数据目录里（先跑 scripts/dm_b1_walk.sh）`).toBeTruthy()
  expect(row!.name, '接口必须给能力名').toBeTruthy()
  expect(row!.name, '名字不能是 id 兜底').not.toBe(DEMO_ID)
  note(`① GET /api/compliance/packs → ${DEMO_ID}: name=${row!.name} summary=${row!.summary}`)
  expect(row!.runtime.available, '装好就应可用（导航据此出现）').toBe(true)

  // ---------- ② 主导航 tab ----------
  render(
    <MemoryRouter>
      <Nav />
    </MemoryRouter>,
  )
  const tab = await waitFor(() => screen.getByTestId(`nav-capability-${DEMO_ID}`))
  expect(tab.textContent).toContain(row!.name!)
  expect(tab.textContent).not.toContain(DEMO_ID)
  expect(tab.getAttribute('title')).toBe(row!.summary)
  note(`② 导航 tab DOM 文本 = 「${tab.textContent}」（title=「${tab.getAttribute('title')}」）`)
  cleanup()

  // ---------- ③ 「送去处理」能力面板 ----------
  render(
    <ToastProvider>
      <MemoryRouter initialEntries={['/?dialog=process&ids=1,2']}>
        <SendToProcessDialog />
      </MemoryRouter>
    </ToastProvider>,
  )
  const capRow = await waitFor(() => screen.getByTestId(`send-process-cap-${DEMO_ID}`))
  expect(capRow.textContent).toContain(row!.name!)
  expect(capRow.textContent).not.toContain(DEMO_ID)
  note(`③ 能力面板行 DOM 文本 = 「${capRow.textContent}」`)
  cleanup()

  // ---------- ④ 能力详情页标题 ----------
  render(
    <ToastProvider>
      <ConfirmProvider>
        <MemoryRouter initialEntries={[`/processing/${encodeURIComponent(DEMO_ID)}`]}>
          <Routes>
            <Route path="/processing/:pluginId" element={<PluginDetail />} />
          </Routes>
        </MemoryRouter>
      </ConfirmProvider>
    </ToastProvider>,
  )
  const title = await waitFor(() => screen.getByTestId('plugin-detail-title'), { timeout: 20000 })
  await waitFor(() => expect(title.textContent).toBe(row!.name))
  note(`④ 详情页标题 DOM 文本 = 「${title.textContent}」`)

  // ---------- ⑤ 卸载确认对话框标题（点开即断言，随后取消） ----------
  const uninstallBtn = await waitFor(() => {
    const btns = screen.getAllByRole('button').filter((b) => b.textContent?.trim() === '卸载')
    expect(btns.length).toBeGreaterThan(0)
    return btns[0]
  })
  fireEvent.click(uninstallBtn)
  const confirmTitle = await screen.findByTestId('confirm-title')
  expect(confirmTitle.textContent).toContain(row!.name!)
  expect(confirmTitle.textContent).not.toContain(DEMO_ID)
  note(`⑤ 卸载确认标题 DOM 文本 = 「${confirmTitle.textContent}」`)
  const cancel = screen.getAllByRole('button').find((b) => b.textContent?.trim() === '取消')
  if (cancel) fireEvent.click(cancel)
  cleanup()

  console.log('\n─── DM-B1 四处 DOM 文本（真实后端）───\n' + log.join('\n') + '\n──────────────────────────────────')
})
