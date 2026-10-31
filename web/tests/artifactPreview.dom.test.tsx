// @vitest-environment jsdom
/**
 * PROC-RES-3（KI-40，卡 task）：处理产物单元格的三件事 ——
 * ① 点「打开」/点缩略图 → **应用内预览**（不是新开窗口）；
 * ② 点「下载」→ toast + 同产物 1.5s 内只触发一次 + 服务端明明失败时要说失败；
 * ③ 「在文件夹中显示」→ 走 `/api/os/reveal`（`mode=reveal`），容器形态（openInOS=false）不渲染按钮。
 *
 * 反向验证口径：把 `claimDownload` 的去重窗口去掉、或把 `onClick` 里的 `preventDefault` 去掉，
 * 用例 4/5 必红 —— 断言盯的是行为，不是「控件存在」。
 */
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { ResultTable } from '../src/components/ResultTable'
import { ToastProvider } from '../src/lib/toast'
import { __setCapabilitiesForTest } from '../src/lib/caps'
import { __resetDownloadGuard } from '../src/lib/artifactActions'
import type { Capabilities } from '../src/lib/api'
import type { ResultRowView } from '../src/lib/resultColumns'

const stubs = vi.hoisted(() => ({
  osReveal: vi.fn(
    async (path: string, mode?: 'reveal' | 'open') =>
      ({
        ok: true,
        path,
        mode: mode ?? 'reveal',
        label: mode === 'open' ? '打开文件夹' : '在文件夹中显示',
        program: 'explorer',
        args: [path],
      }) as const,
  ),
}))

vi.mock('../src/lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../src/lib/api')>()
  return { ...actual, api: { ...actual.api, osReveal: stubs.osReveal } }
})

const ART = '/data/state/compliance/fixes/job-9/done_x4.webp'

const rows: ResultRowView[] = [
  { assetId: 30, fileName: 'a.jpg', ruleId: 'r1', result: 'ok', verdict: null, fields: {}, artifacts: [ART] },
]

function capsWith(openInOS: boolean): Capabilities {
  return {
    isDesktop: openInOS,
    storeMode: openInOS ? 'local-disk' : 'server-volume',
    writableRoots: [],
    roots: [],
    features: { openInOS } as Capabilities['features'],
    featureReasons: openInOS ? {} : { openInOS: '文件在后端服务器上（网页形态不能直接打开本机文件夹）' },
  } as Capabilities
}

function renderTable(openInOS = true) {
  __setCapabilitiesForTest(capsWith(openInOS))
  return render(
    <ToastProvider>
      <ResultTable testid="t" rows={rows} columns={null} kinds={['process']} />
    </ToastProvider>,
  )
}

const tid = (suffix: string) => `t-artifact-${suffix}-30-0`

beforeEach(() => {
  __resetDownloadGuard()
  stubs.osReveal.mockClear()
  vi.stubGlobal(
    'fetch',
    vi.fn(async () => ({ ok: true, status: 200 }) as Response),
  )
})

afterEach(() => {
  cleanup()
  __setCapabilitiesForTest(null)
  vi.unstubAllGlobals()
})

describe('PROC-RES-3 产物应用内预览', () => {
  it('点「打开」→ 预览浮层出现（img 读同源产物 URL），且不再新开窗口', () => {
    renderTable()
    const open = screen.getByTestId(tid('open'))
    // 不再有 target=_blank（新开标签页 = 在桌面壳里把用户甩出应用）
    expect(open.getAttribute('target')).toBeNull()

    fireEvent.click(open)

    const dlg = screen.getByTestId('t-artifact-preview')
    expect(dlg).toBeTruthy()
    const img = screen.getByTestId('t-artifact-preview-image') as HTMLImageElement
    expect(img.getAttribute('src')).toContain('/api/compliance/artifact?path=')
    expect(img.getAttribute('src')).toContain(encodeURIComponent(ART))
  })

  it('点缩略图 / Esc 关闭：浮层消失，表格行还在（不丢上下文）', async () => {
    renderTable()
    fireEvent.click(screen.getByTestId(tid('thumb')))
    expect(screen.getByTestId('t-artifact-preview')).toBeTruthy()

    fireEvent.keyDown(window, { key: 'Escape' })
    await waitFor(() => expect(screen.queryByTestId('t-artifact-preview')).toBeNull())
    expect(screen.getByTestId('t-cell-30-__artifacts')).toBeTruthy()
  })
})

describe('PROC-RES-3 下载：反馈 + 去重 + 失败要说话', () => {
  it('点下载 → 有 toast；1.5s 内重复点击被拦（只探活一次、第二次 defaultPrevented）', async () => {
    renderTable()
    const dl = screen.getByTestId(tid('download'))
    fireEvent.click(dl)
    expect(await screen.findByText('开始下载')).toBeTruthy()
    expect(vi.mocked(fetch).mock.calls.length).toBe(1)

    // 同一产物立刻再点：浏览器不该再起一个下载任务
    const ev = new MouseEvent('click', { bubbles: true, cancelable: true })
    dl.dispatchEvent(ev)
    expect(ev.defaultPrevented).toBe(true)
    expect(vi.mocked(fetch).mock.calls.length).toBe(1)
    expect(await screen.findByText('已在下载')).toBeTruthy()
  })

  it('服务端 404（产物已不在磁盘）→ error toast，不静默', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => ({ ok: false, status: 404 }) as Response),
    )
    renderTable()
    fireEvent.click(screen.getByTestId(tid('download')))
    expect(await screen.findByText('下载失败')).toBeTruthy()
    expect(screen.getByText(/产物文件已不在磁盘/)).toBeTruthy()
  })
})

describe('PROC-RES-3 在文件夹中显示：形态门控 + 唯一接口', () => {
  it('桌面形态：按钮在，点了调 os/reveal(mode=reveal)', async () => {
    renderTable(true)
    const btn = screen.getByTestId(tid('reveal'))
    fireEvent.click(btn)
    await waitFor(() => expect(stubs.osReveal).toHaveBeenCalledWith(ART, 'reveal'))
    // toast 标题取自服务端 `label`（前端不另写一份文案），细节里带文件与路径
    expect(await screen.findByText(`done_x4.webp —— ${ART}`)).toBeTruthy()
  })

  it('容器形态：不渲染按钮（灰按钮能点会被骂），预览浮层里给原因', () => {
    renderTable(false)
    expect(screen.queryByTestId(tid('reveal'))).toBeNull()

    fireEvent.click(screen.getByTestId(tid('open')))
    expect(screen.getByTestId('t-artifact-preview')).toBeTruthy()
    // PathLink 在形态不支持时给的是人话原因，不是灰按钮
    expect(screen.getByText(/网页形态不能直接打开本机文件夹/)).toBeTruthy()
  })
})
