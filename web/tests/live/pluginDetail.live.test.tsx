// @vitest-environment jsdom
import { afterAll, beforeAll, expect, it } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter, Route, Routes, useLocation } from 'react-router-dom'
import { ToastProvider } from '../../src/lib/toast'
import { ConfirmProvider } from '../../src/lib/confirm'
import { PluginDetail } from '../../src/pages/PluginDetail'

/**
 * 手验档（卡 t_d927e0d4）：**不打桩**，全部走真实 `app-web`。
 *
 * 链路与卡上的手验要求一一对应（顺序就是用户操作顺序）：
 *   ① 打开插件详情页 → 三段都从真实接口来（packs / pack detail / schema / specs）
 *   ② 新建方案（同一份 ParamForm，只提交改过的键）→ POST /api/specs 真落库 → 列表出现
 *   ③ 先试一张 → POST /api/specs/{id}/try 真入队 → 单张结果**就地显示**（KI-35 P2：不再跳走）
 *   ④ 这一次 run 真的能查（GET /api/compliance/jobs/{runId}），整批入口带 runId
 *   ⑤ 收尾删掉本次建的方案（不留垃圾数据）
 *
 * 期望值全是**这个部署的真实值**（不是写死的假数据）：插件 id/版本从 /api/compliance/packs 读，
 * 参数默认值从 schema 读。所以包升级后这里不用改。
 */

// 不用 `process`（web tsconfig 只有 DOM lib，没有 @types/node）：手验档靠 env 覆盖即可
const ENV = ((globalThis as { process?: { env?: Record<string, string | undefined> } }).process?.env ??
  {}) as Record<string, string | undefined>
const BASE = ENV.DAM_WEB_BASE ?? 'http://127.0.0.1:8791'
const PLUGIN_ID = ENV.DAM_PLUGIN_ID ?? 'com.soujpg.pack.ecommerce'
const realFetch = globalThis.fetch.bind(globalThis)

// 相对路径 → 真后端（web 里 `api` 用的是 `/api/...`，浏览器靠同源，这里补 host）
globalThis.fetch = ((input: RequestInfo | URL, init?: RequestInit) => {
  if (typeof input === 'string' && input.startsWith('/')) return realFetch(BASE + input, init)
  return realFetch(input as RequestInfo, init)
}) as typeof fetch

const log: string[] = []
function note(line: string) {
  log.push(line)
}

function LocationSpy() {
  const loc = useLocation()
  return <div data-testid="loc">{`${loc.pathname}${loc.search}`}</div>
}

let createdSpecId: number | null = null

beforeAll(async () => {
  const r = await fetch('/api/healthz')
  expect(r.ok).toBe(true)
  note(`backend ${BASE} healthz=${r.status}`)
})

afterAll(async () => {
  // 收尾：删掉本次新建的规格（判定历史/产物本来就不受删规格影响）
  if (createdSpecId != null) {
    const r = await fetch(`/api/specs/${createdSpecId}`, { method: 'DELETE' })
    note(`cleanup DELETE /api/specs/${createdSpecId} → ${r.status}`)
  }
  cleanup()
  // 把整条手验链路打进测试输出（报告直接引用）
  console.log('\n─── 手验链路（真实后端）───\n' + log.join('\n') + '\n────────────────────────────')
})

it('真实后端：详情页三段 → 新建规格 → 先跑 1 张 → 带到这次 run 的结果面', async () => {
  // ---------- ① 真实数据先摸一遍（外部证据与界面断言用同一份数据） ----------
  const packs = (await (await fetch('/api/compliance/packs')).json()) as {
    plugins: { id: string; currentVersion: string | null; builtin: boolean }[]
  }
  const pack = packs.plugins.find((p) => p.id === PLUGIN_ID)
  expect(pack, `插件 ${PLUGIN_ID} 应装在这次手验的数据目录里`).toBeTruthy()
  note(`packs: ${PLUGIN_ID} builtin=${pack!.builtin} current=${pack!.currentVersion}`)

  const schemaView = (await (await fetch(`/api/plugins/${PLUGIN_ID}/schema`)).json()) as {
    declared: boolean
    schema: { properties: Record<string, { default?: unknown; title?: string }> }
  }
  expect(schemaView.declared).toBe(true)
  const schemaKeys = Object.keys(schemaView.schema.properties)
  note(`schema declared=true keys=${schemaKeys.join(',')}`)

  const before = (await (await fetch(`/api/specs?plugin=${PLUGIN_ID}`)).json()) as {
    total: number
    items: { specId: number; name: string }[]
  }
  note(`specs before: total=${before.total} names=${before.items.map((s) => s.name).join(' / ')}`)

  // ---------- ① 渲染（真实接口，无 mock） ----------
  render(
    <MemoryRouter initialEntries={[`/plugins/${PLUGIN_ID}`]}>
      <ToastProvider>
        <ConfirmProvider>
          <Routes>
            <Route path="/plugins/:pluginId" element={<PluginDetail />} />
            <Route path="/compliance" element={<div data-testid="compliance-page">合规审核页</div>} />
          </Routes>
          <LocationSpy />
        </ConfirmProvider>
      </ToastProvider>
    </MemoryRouter>,
  )

  await screen.findByTestId('plugin-section-capability', undefined, { timeout: 20000 })
  const title = screen.getByTestId('plugin-detail-title').textContent ?? ''
  const runtime = screen.getByTestId('plugin-detail-runtime').textContent ?? ''
  note(`① 能力说明: title="${title}" runtime="${runtime.trim()}" outputs=${screen.getByTestId('plugin-detail-outputs').textContent}`)
  for (const key of ['what', 'needs', 'gives', 'engine', 'version']) {
    expect(screen.getByTestId(`plugin-cap-${key}`), `能力说明缺 ${key}`).toBeTruthy()
  }
  note(`① 版本行: ${screen.getByTestId('plugin-cap-version').textContent}`)

  // 规格列表 = 真实库里那几条
  await waitFor(() => expect(screen.getByTestId('plugin-section-specs')).toBeTruthy())
  const specRows = document.querySelectorAll('[data-testid^="spec-row-"]')
  expect(specRows.length).toBe(before.total)
  note(`① 我的方案: ${specRows.length} 条（与 /api/specs 一致）`)

  // 默认设置 = 官方推荐参数（不是某个方案的 diff）
  expect(screen.getByTestId('plugin-paramform-defaults-note')).toBeTruthy()
  const firstKey = schemaKeys[0]
  const defaultInput = screen.getByTestId(`plugin-paramform-input-${firstKey}`) as HTMLInputElement
  expect(defaultInput.disabled).toBe(true)
  note(`① 参数默认值: ${firstKey} = ${defaultInput.value}（只读）`)

  // ---------- ② 新建方案（真 POST） ----------
  const specName = `手验方案-${Date.now()}`
  fireEvent.click(screen.getByTestId('plugin-spec-new'))
  await screen.findByTestId('spec-editor')
  fireEvent.change(screen.getByTestId('spec-name'), { target: { value: specName } })
  // 改一个参数（只这一个键会进 paramsDiff）
  const editorInput = screen.getByTestId(`spec-paramform-input-${firstKey}`) as HTMLInputElement
  const original = Number(schemaView.schema.properties[firstKey].default)
  const changed = Number.isFinite(original) ? original + 1 : 42
  fireEvent.change(editorInput, { target: { value: String(changed) } })
  fireEvent.click(screen.getByTestId('spec-editor-save'))

  await waitFor(
    () => {
      const names = Array.from(document.querySelectorAll('[data-testid^="spec-name-"]')).map(
        (n) => n.textContent ?? '',
      )
      expect(names).toContain(specName)
    },
    { timeout: 20000 },
  )
  const after = (await (await fetch(`/api/specs?plugin=${PLUGIN_ID}`)).json()) as {
    total: number
    items: { specId: number; name: string; paramsDiff: Record<string, unknown> }[]
  }
  const created = after.items.find((s) => s.name === specName)
  expect(created, '新建的方案应落在真库里').toBeTruthy()
  createdSpecId = created!.specId
  expect(created!.paramsDiff).toEqual({ [firstKey]: changed })
  note(`② 新建方案: specId=${created!.specId} paramsDiff=${JSON.stringify(created!.paramsDiff)}（total ${before.total} → ${after.total}）`)

  // ---------- ③ 先试一张（真 /try，KI-35 P2：结果就地显示，不再跳走） ----------
  // 让这条新方案成为默认（避免用到别的方案的阈值配置）
  const newRowBtn = (await screen.findByTestId(`spec-default-set-${createdSpecId}`)) as HTMLButtonElement
  fireEvent.click(newRowBtn)
  await screen.findByTestId(`spec-default-${createdSpecId}`)

  fireEvent.click(screen.getByTestId('plugin-try-one'))
  // 就地面板出现（running → done 由页面自己轮询；真判定几秒到几十秒）
  await screen.findByTestId('plugin-try-result', undefined, { timeout: 30000 })
  await waitFor(
    () => {
      expect(screen.queryByTestId('plugin-try-running')).toBeNull()
      expect(screen.queryByTestId('plugin-try-error')).toBeNull()
    },
    { timeout: 120000 },
  )
  const openLink = screen.getByTestId('plugin-try-open-record').getAttribute('href') ?? ''
  const runId = Number(openLink.split('task=')[1])
  expect(Number.isFinite(runId) && runId > 0).toBe(true)
  // 留在本页；单张结论就地可见（判定类给 通过/未通过/待人工看）
  expect(screen.getByTestId('loc').textContent ?? '').toBe(`/plugins/${PLUGIN_ID}`)
  const rows = document.querySelectorAll('[data-testid^="plugin-try-row-"]')
  expect(rows.length).toBeGreaterThan(0)
  note(`③ 先试一张: runId=${runId} 就地 ${rows.length} 行结论（首个：${rows[0].textContent}）`)

  // ---------- ④ 这一次 run 真的能查 ----------
  const jobResp = await fetch(`/api/compliance/jobs/${runId}`)
  expect(jobResp.ok).toBe(true)
  const job = (await jobResp.json()) as {
    run?: { runId: number; status: string; total: number; done: number; failed: number; skipped: number; queued?: number }
    items?: unknown[]
  }
  note(`④ 处理记录 GET /api/compliance/jobs/${runId} → ${jobResp.status} run=${JSON.stringify(job.run)}`)
  expect(job.run?.runId).toBe(runId)
  // 引擎=plugin 时这次 run 的插件参数快照要落库（M: 手验证明「生效参数」可复盘）
  const specAfter = (await (await fetch(`/api/specs/${createdSpecId}`)).json()) as {
    name: string
    paramsDiff: Record<string, unknown>
  }
  expect(specAfter.paramsDiff).toEqual({ [firstKey]: changed })
  note(`④ 方案回读: ${specAfter.name} paramsDiff=${JSON.stringify(specAfter.paramsDiff)}`)
})
