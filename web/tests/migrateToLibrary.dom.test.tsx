// @vitest-environment jsdom
/**
 * DT7（卡 task）：一键迁入库·前端入口。
 *
 * 断言的是**真实 DOM + 真实调用入参**（`../src/lib/api` 被替身，页面/浮层逻辑全跑）：
 *  1. 工具条「迁入库」：无选择 / 无可用库根 / 选中项全不可迁 → 禁用 + 原因文案；
 *  2. URL 驱动（`?dialog=migrate-to-library&ids=…`）可直达，标题带张数、目标库根来自 capabilities.roots；
 *  3. 只提交可迁移项（reference + 未失联 + 有 fileHash），不可迁项逐条列原因；
 *  4. 「迁完删源」默认关 + 二次确认（未确认不发请求）；
 *  5. 轮询 status 出进度 → 结果（成功/跳过逐条原因/失败/告警）；
 *  6. 失败路径：409 冲突、资产读取失败（重试）、后端快照丢失（idle）、整批失败、轮询报错。
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter, useLocation } from 'react-router-dom'
import { ToastProvider } from '../src/lib/toast'
import { TaskCenterProvider } from '../src/lib/tasks'
import { ConfirmProvider } from '../src/lib/confirm'
import { __setCapabilitiesForTest } from '../src/lib/caps'
import { clearGallerySnapshot } from '../src/lib/galleryCache'
import {
  isMigratable,
  migrateHoldReason,
  migrateSkipText,
} from '../src/components/MigrateToLibraryDialog'
import type { AssetView, Capabilities } from '../src/lib/api'

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

// ---- jsdom 存储桩（Gallery 首屏读偏好；本仓既有测试同样手动打桩）----
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

const calls = vi.hoisted(() => ({
  assets: vi.fn(),
  migrateToLibrary: vi.fn(),
  migrateToLibraryStatus: vi.fn(),
  capabilities: vi.fn(),
}))

vi.mock('../src/lib/api', async (importOriginal) => {
  const actual = await importOriginal<Record<string, unknown>>()
  const stubs: Record<string, unknown> = {
    stats: async () => ({ assets: 3, totalBytes: 0, dirs: 1, tags: 0 }),
    dirs: async () => ({ dirs: [], roots: [] }),
    tags: async () => [],
    cameras: async () => ({ cameras: [] }),
    complianceVerdicts: async () => null,
    complianceReview: async () => ({ threshold: 0.7, items: [] }),
    assetTags: async () => [],
    complianceStatus: async () => ({ states: {}, pack: { id: 'none' }, verdictPackId: 'none' }),
    importStatus: async () => ({ running: false, progress: null }),
    capabilities: calls.capabilities,
    thumbUrl: (id: number) => `/api/media/thumb/${id}`,
    originalUrl: (id: number) => `/api/media/original/${id}`,
    assets: calls.assets,
    migrateToLibrary: calls.migrateToLibrary,
    migrateToLibraryStatus: calls.migrateToLibraryStatus,
  }
  const api = new Proxy(stubs, {
    get(target, key: string) {
      if (key in target) return target[key]
      // 未用到的接口：安全空实现（页面里都带 catch，不影响断言）
      return async () => ({})
    },
  })
  return { ...actual, api }
})

function asset(id: number, over: Partial<AssetView> = {}): AssetView {
  return {
    assetId: id,
    mediaType: 'image',
    absPath: `/photos/a${id}.jpg`,
    storeMode: 'reference',
    missing: 0,
    fileName: `a${id}.jpg`,
    fileExt: 'jpg',
    fileSz: 1024,
    mtime: 0,
    fileHash: `hash-${id}`,
    titleCh: null,
    titleEn: null,
    description: null,
    source: 'import',
    parentAssetId: null,
    searchable: 1,
    deleted: 0,
    approvalStatus: 'none',
    starRating: 0,
    colorLabel: null,
    cloudAssetId: null,
    importedAt: id,
    embStatus: 'done',
    embModel: 'siglip2',
    dedupStatus: 'done',
    dedupModel: 'dinov3',
    thumbPath: null,
    width: 10,
    height: 10,
    pHash: null,
    takenAt: null,
    cameraMake: null,
    cameraModel: null,
    lensModel: null,
    iso: null,
    focalLenMm: null,
    aperture: null,
    shutterText: null,
    orientation: null,
    aspectRatio: null,
    aspectClass: null,
    gpsLat: null,
    gpsLon: null,
    gpsSource: null,
    exif: null,
    ...over,
  } as AssetView
}

const capsWithRoot: Capabilities = {
  isDesktop: false,
  storeMode: 'server-volume',
  writableRoots: ['/data/library'],
  roots: [{ path: '/data/library', writable: true }],
  features: {
    moveAsset: true,
    renameAsset: true,
    relink: true,
    openInOS: true,
    physicalDelete: true,
    copyInImport: true,
    scanExternalPath: true,
  },
  featureReasons: {},
}

function LocationProbe() {
  const loc = useLocation()
  return <span data-testid="loc">{loc.pathname + loc.search}</span>
}

async function renderDialog(entry: string, onFinished?: () => void) {
  const { MigrateToLibraryDialog } = await import('../src/components/MigrateToLibraryDialog')
  return render(
    <ToastProvider>
      <MemoryRouter initialEntries={[entry]}>
        <LocationProbe />
        <MigrateToLibraryDialog onFinished={onFinished} />
      </MemoryRouter>
    </ToastProvider>,
  )
}

async function renderGallery(initial: string) {
  const { Gallery } = await import('../src/pages/Gallery')
  return render(
    <ToastProvider>
      <TaskCenterProvider>
        <ConfirmProvider>
          <MemoryRouter initialEntries={[initial]}>
            <LocationProbe />
            <Gallery onImport={() => {}} />
          </MemoryRouter>
        </ConfirmProvider>
      </TaskCenterProvider>
    </ToastProvider>,
  )
}

beforeAll(() => {
  Object.defineProperty(window, 'matchMedia', {
    writable: true,
    value: (query: string) => ({
      matches: true,
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    }),
  })
  Object.defineProperty(window, 'innerWidth', { writable: true, configurable: true, value: 1600 })
  Object.defineProperty(window, 'innerHeight', { writable: true, configurable: true, value: 1000 })
})

beforeEach(() => {
  // E1 快照是**模块级单例**（lib/galleryCache.ts），不随 cleanup 消失：不清的话下一条用例会
  // 「恢复」上一条的 items，按钮禁用态就不是本用例的数据算出来的
  // （实测踩到：copyin 用例拿到上一条 reference 图库的快照 → 按钮没灰）。
  clearGallerySnapshot()
  localStorage.clear()
  sessionStorage.clear()
  calls.assets.mockReset()
  calls.migrateToLibrary.mockReset()
  calls.migrateToLibraryStatus.mockReset()
  calls.capabilities.mockReset()
  calls.migrateToLibraryStatus.mockResolvedValue({ state: 'idle' })
  // 能力开关是模块级缓存：每条用例显式给一份（否则上一条的形态会漏到下一条）
  __setCapabilitiesForTest(capsWithRoot)
})

afterEach(() => {
  cleanup()
  __setCapabilitiesForTest(null)
  vi.clearAllMocks()
  vi.unstubAllGlobals()
})

// ───────────────────────── 纯函数（判定与文案） ─────────────────────────

describe('DT7 纯函数：可迁移判定 / 原因文案', () => {
  it('isMigratable：只认 reference + 未失联 + 有 fileHash', () => {
    expect(isMigratable(asset(1))).toBe(true)
    expect(isMigratable(asset(1, { storeMode: 'copyin' }))).toBe(false)
    expect(isMigratable(asset(1, { missing: 1 }))).toBe(false)
    expect(isMigratable(asset(1, { fileHash: null }))).toBe(false)
    expect(isMigratable(asset(1, { fileHash: '' }))).toBe(false)
  })

  it('migrateHoldReason：失联 / 缺指纹 / 已入库 三档，可迁为空串', () => {
    expect(migrateHoldReason(asset(1))).toBe('')
    expect(migrateHoldReason(asset(1, { missing: 1 }))).toContain('失联')
    expect(migrateHoldReason(asset(1, { fileHash: null }))).toContain('内容指纹')
    expect(migrateHoldReason(asset(1, { storeMode: 'copyin' }))).toContain('已是库内保管')
  })

  it('migrateSkipText：后端五档 code 全有人话，未知 code 不吞（回显 code）', () => {
    for (const code of ['not_found', 'no_hash', 'already_copyin', 'missing_file', 'content_changed']) {
      expect(migrateSkipText(code).startsWith('跳过（')).toBe(false)
      expect(migrateSkipText(code)).not.toBe('')
    }
    expect(migrateSkipText('weird_new_code')).toBe('跳过（weird_new_code）')
  })
})

// ───────────────────────── api 契约（真模块 + fetch 桩） ─────────────────────────

describe('DT7 api 契约', () => {
  function okJson(body: unknown, status = 202, statusText = 'Accepted') {
    return {
      ok: status < 400,
      status,
      statusText,
      json: async () => body,
    } as unknown as Response
  }

  it('migrateToLibrary：POST + body 原样透传（ids/deleteSource）', async () => {
    const fetchMock = vi.fn(async (_url: string, _init?: RequestInit) =>
      okJson({ batchId: 'b1', state: 'running', total: 2 }),
    )
    vi.stubGlobal('fetch', fetchMock)
    const actual = await vi.importActual<typeof import('../src/lib/api')>('../src/lib/api')
    const r = await actual.api.migrateToLibrary({ ids: [1, 2], deleteSource: true })
    expect(r).toEqual({ batchId: 'b1', state: 'running', total: 2 })
    const [url, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit]
    expect(url).toBe('/api/library/migrate-to-library')
    expect(init.method).toBe('POST')
    expect(JSON.parse(String(init.body))).toEqual({ ids: [1, 2], deleteSource: true })
  })

  it('migrateToLibraryStatus：无 batchId 不带 query；有 batchId 时编码进 query', async () => {
    const fetchMock = vi.fn(async (_url: string) => ({
      ok: true,
      status: 200,
      statusText: 'OK',
      json: async () => ({ state: 'idle' }),
    }) as unknown as Response)
    vi.stubGlobal('fetch', fetchMock)
    const actual = await vi.importActual<typeof import('../src/lib/api')>('../src/lib/api')

    await actual.api.migrateToLibraryStatus()
    expect(fetchMock.mock.calls[0][0]).toBe('/api/library/migrate-to-library/status')

    await actual.api.migrateToLibraryStatus('mig 1/2')
    expect(fetchMock.mock.calls[1][0]).toBe(
      '/api/library/migrate-to-library/status?batchId=mig%201%2F2',
    )
  })

  it('migrateToLibrary：409 冲突抛错，人话取自后端 error 字段（不吞成状态码）', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => ({
        ok: false,
        status: 409,
        statusText: 'Conflict',
        json: async () => ({ error: '已有迁移任务在跑，等它结束再提交', batchId: 'old' }),
      }) as unknown as Response),
    )
    const actual = await vi.importActual<typeof import('../src/lib/api')>('../src/lib/api')
    await expect(actual.api.migrateToLibrary({ ids: [1] })).rejects.toThrow(
      '已有迁移任务在跑，等它结束再提交',
    )
  })
})

// ───────────────────────── 对话框 ─────────────────────────

describe('DT7 对话框：确认 → 进度 → 结果', () => {
  it('标题带张数；目标库根来自 capabilities.roots；可迁/不迁与逐条原因都列出来', async () => {
    calls.assets.mockResolvedValue({
      items: [asset(1), asset(2), asset(3, { storeMode: 'copyin' }), asset(4, { missing: 1 })],
      total: 4,
    })
    await renderDialog('/gallery?dialog=migrate-to-library&ids=1,2,3,4')

    await waitFor(() => expect(screen.getByTestId('migrate-summary')).toBeTruthy())
    expect(screen.getByTestId('migrate-dialog-title').textContent).toBe('迁入库（4 张）')
    expect(screen.getByTestId('migrate-root').textContent).toBe('/data/library')
    expect(screen.getByTestId('migrate-eligible').textContent).toBe('2')
    expect(screen.getByTestId('migrate-held').textContent).toBe('2')
    const held = screen.getByTestId('migrate-held-list').textContent ?? ''
    expect(held).toContain('#3')
    expect(held).toContain('已是库内保管')
    expect(held).toContain('#4')
    expect(held).toContain('失联')
    // 源保留是默认：删源复选框未勾
    expect((screen.getByTestId('migrate-delete-source') as HTMLInputElement).checked).toBe(false)
    expect((screen.getByTestId('migrate-start') as HTMLButtonElement).disabled).toBe(false)
  })

  it('不可迁移项超过 10 条 → 折成「还有 N 张」，不无限堆叠', async () => {
    const ids = Array.from({ length: 12 }, (_, i) => i + 1)
    calls.assets.mockResolvedValue({
      items: ids.map((id) => (id === 1 ? asset(1) : asset(id, { storeMode: 'copyin' }))),
      total: ids.length,
    })
    await renderDialog(`/gallery?dialog=migrate-to-library&ids=${ids.join(',')}`)
    await waitFor(() => expect(screen.getByTestId('migrate-held-list')).toBeTruthy())
    const list = screen.getByTestId('migrate-held-list').textContent ?? ''
    expect(list).toContain('还有 1 张')
    expect(list.match(/#/g)?.length).toBe(10) // 只列前 10 条
  })

  it('提交只发可迁项；轮询出进度条 → 结果逐条原因；完成后回调 onFinished', async () => {
    const onFinished = vi.fn()
    calls.assets.mockResolvedValue({
      items: [asset(1), asset(2), asset(3, { storeMode: 'copyin' })],
      total: 3,
    })
    calls.migrateToLibrary.mockResolvedValue({ batchId: 'b1', state: 'running', total: 2 })
    let tick = 0
    calls.migrateToLibraryStatus.mockImplementation(async (batchId?: string) => {
      if (!batchId) return { state: 'idle' }
      tick += 1
      if (tick === 1) {
        return { state: 'running', batchId, total: 2, processed: 1, migrated: 1, skipped: 0, failed: 0, bytes: 0 }
      }
      return {
        state: 'done',
        batchId,
        total: 2,
        processed: 2,
        migrated: 1,
        skipped: 1,
        failed: 1,
        bytes: 2048,
        report: {
          batchId,
          total: 2,
          migrated: 1,
          skipped: [{ assetId: 2, reason: 'content_changed', detail: '磁盘文件内容与入库指纹不一致（被替换过）' }],
          failed: [{ assetId: 5, code: 'copy_failed', detail: 'ENOSPC' }],
          bytes: 2048,
          warnings: [{ assetId: 1, detail: '源文件删不掉（权限）' }],
        },
      }
    })

    await renderDialog('/gallery?dialog=migrate-to-library&ids=1,2,3', onFinished)
    await waitFor(() => expect(screen.getByTestId('migrate-summary')).toBeTruthy())
    fireEvent.submit(screen.getByTestId('migrate-form'))

    await waitFor(() =>
      expect(calls.migrateToLibrary).toHaveBeenCalledWith({ ids: [1, 2], deleteSource: false }),
    )
    // 进度条（先看到 running；processed 由轮询推进）
    await waitFor(() => expect(screen.getByTestId('migrate-progress')).toBeTruthy())
    await waitFor(() =>
      expect(screen.getByTestId('migrate-progress').textContent).toContain('1/2'),
    )

    await waitFor(() => expect(screen.getByTestId('migrate-report')).toBeTruthy(), { timeout: 3000 })
    const rep = screen.getByTestId('migrate-report').textContent ?? ''
    expect(rep).toContain('成功')
    expect(rep).toContain('跳过 1')
    expect(rep).toContain('失败 1')
    expect(rep).toContain('2 KB')
    const skipped = screen.getByTestId('migrate-skipped').textContent ?? ''
    expect(skipped).toContain('#2')
    expect(skipped).toContain('磁盘文件内容与入库指纹不一致')
    expect(screen.getByTestId('migrate-failed').textContent).toContain('ENOSPC')
    expect(screen.getByTestId('migrate-warnings').textContent).toContain('源文件删不掉')
    expect(onFinished).toHaveBeenCalled()
  })

  it('勾「迁完删源」→ 先弹二次确认（不发请求）；取消/未确认都不提交，确认后才带 deleteSource:true', async () => {
    calls.assets.mockResolvedValue({ items: [asset(1), asset(2)], total: 2 })
    calls.migrateToLibrary.mockResolvedValue({ batchId: 'b2', state: 'running', total: 2 })

    await renderDialog('/gallery?dialog=migrate-to-library&ids=1,2')
    await waitFor(() => expect(screen.getByTestId('migrate-summary')).toBeTruthy())

    fireEvent.click(screen.getByTestId('migrate-delete-source'))
    fireEvent.submit(screen.getByTestId('migrate-form'))
    await waitFor(() => expect(screen.getByTestId('migrate-delete-confirm')).toBeTruthy())
    expect(calls.migrateToLibrary).not.toHaveBeenCalled()

    // 取消 → 二次确认收起，仍不发请求
    fireEvent.click(screen.getByTestId('migrate-delete-confirm-cancel'))
    await waitFor(() => expect(screen.queryByTestId('migrate-delete-confirm')).toBeNull())
    expect(calls.migrateToLibrary).not.toHaveBeenCalled()

    // 再次提交 → 二次确认 → 确认后才真发
    fireEvent.submit(screen.getByTestId('migrate-form'))
    await waitFor(() => expect(screen.getByTestId('migrate-delete-confirm')).toBeTruthy())
    fireEvent.click(screen.getByTestId('migrate-delete-confirm-ok'))
    await waitFor(() =>
      expect(calls.migrateToLibrary).toHaveBeenCalledWith({ ids: [1, 2], deleteSource: true }),
    )
  })

  it('启动失败（409 冲突）→ 错误文案可见，不进轮询', async () => {
    calls.assets.mockResolvedValue({ items: [asset(1)], total: 1 })
    calls.migrateToLibrary.mockRejectedValue(new Error('已有迁移任务在跑，等它结束再提交'))

    await renderDialog('/gallery?dialog=migrate-to-library&ids=1')
    await waitFor(() => expect(screen.getByTestId('migrate-summary')).toBeTruthy())
    fireEvent.submit(screen.getByTestId('migrate-form'))

    await waitFor(() => expect(screen.getByTestId('migrate-error')).toBeTruthy())
    expect(screen.getByTestId('migrate-error').textContent).toContain('已有迁移任务在跑')
    expect(screen.queryByTestId('migrate-progress')).toBeNull()
  })

  it('选中资产读取失败 → 报错 + 重试（重试成功后恢复可就绪）', async () => {
    calls.assets.mockRejectedValueOnce(new Error('boom'))
    await renderDialog('/gallery?dialog=migrate-to-library&ids=1')
    await waitFor(() => expect(screen.getByTestId('migrate-error')).toBeTruthy())
    expect(screen.getByTestId('migrate-error').textContent).toContain('boom')
    expect(screen.queryByTestId('migrate-summary')).toBeNull()
    expect((screen.getByTestId('migrate-start') as HTMLButtonElement).disabled).toBe(true)

    calls.assets.mockResolvedValue({ items: [asset(1)], total: 1 })
    fireEvent.click(screen.getByTestId('migrate-reload'))
    await waitFor(() => expect(screen.getByTestId('migrate-summary')).toBeTruthy())
    expect((screen.getByTestId('migrate-start') as HTMLButtonElement).disabled).toBe(false)
  })

  it('选中项全不可迁 → 开始按钮禁用 + 原因；提交也不发请求', async () => {
    calls.assets.mockResolvedValue({ items: [asset(1, { storeMode: 'copyin' })], total: 1 })
    await renderDialog('/gallery?dialog=migrate-to-library&ids=1')
    await waitFor(() => expect(screen.getByTestId('migrate-summary')).toBeTruthy())
    const start = screen.getByTestId('migrate-start') as HTMLButtonElement
    expect(start.disabled).toBe(true)
    expect(start.title).toContain('无需迁移')
    fireEvent.submit(screen.getByTestId('migrate-form'))
    expect(calls.migrateToLibrary).not.toHaveBeenCalled()
  })

  it('没有选择集（URL 无 ids）→ 按钮禁用 + 空张数标题，不请求资产', async () => {
    await renderDialog('/gallery?dialog=migrate-to-library')
    expect(screen.getByTestId('migrate-dialog-title').textContent).toBe('迁入库（0 张）')
    const start = screen.getByTestId('migrate-start') as HTMLButtonElement
    expect(start.disabled).toBe(true)
    expect(start.title).toContain('没有选择集')
    expect(calls.assets).not.toHaveBeenCalled()
  })

  it('轮询期间后端快照丢失（idle）→ 明确报错，不装成完成', async () => {
    calls.assets.mockResolvedValue({ items: [asset(1)], total: 1 })
    calls.migrateToLibrary.mockResolvedValue({ batchId: 'gone', state: 'running', total: 1 })
    calls.migrateToLibraryStatus.mockImplementation(async (batchId?: string) =>
      batchId ? { state: 'idle', batchId } : { state: 'idle' },
    )
    await renderDialog('/gallery?dialog=migrate-to-library&ids=1')
    await waitFor(() => expect(screen.getByTestId('migrate-summary')).toBeTruthy())
    fireEvent.submit(screen.getByTestId('migrate-form'))

    await waitFor(() => expect(screen.getByTestId('migrate-error')).toBeTruthy(), { timeout: 3000 })
    expect(screen.getByTestId('migrate-error').textContent).toContain('进度记录')
    expect(screen.queryByTestId('migrate-report')).toBeNull()
  })

  it('整批失败（state=failed）→ 显示后端错误原文', async () => {
    calls.assets.mockResolvedValue({ items: [asset(1)], total: 1 })
    calls.migrateToLibrary.mockResolvedValue({ batchId: 'bad', state: 'running', total: 1 })
    calls.migrateToLibraryStatus.mockImplementation(async (batchId?: string) =>
      batchId ? { state: 'failed', batchId, error: '库根不可写' } : { state: 'idle' },
    )
    await renderDialog('/gallery?dialog=migrate-to-library&ids=1')
    await waitFor(() => expect(screen.getByTestId('migrate-summary')).toBeTruthy())
    fireEvent.submit(screen.getByTestId('migrate-form'))

    await waitFor(() => expect(screen.getByTestId('migrate-batch-failed')).toBeTruthy(), { timeout: 3000 })
    expect(screen.getByTestId('migrate-batch-failed').textContent).toContain('库根不可写')
  })

  it('轮询请求本身报错 → 停止轮询并显示错误', async () => {
    calls.assets.mockResolvedValue({ items: [asset(1)], total: 1 })
    calls.migrateToLibrary.mockResolvedValue({ batchId: 'x', state: 'running', total: 1 })
    calls.migrateToLibraryStatus.mockImplementation(async (batchId?: string) => {
      if (!batchId) return { state: 'idle' }
      throw new Error('network down')
    })
    await renderDialog('/gallery?dialog=migrate-to-library&ids=1')
    await waitFor(() => expect(screen.getByTestId('migrate-summary')).toBeTruthy())
    fireEvent.submit(screen.getByTestId('migrate-form'))

    await waitFor(() => expect(screen.getByTestId('migrate-error')).toBeTruthy(), { timeout: 3000 })
    expect(screen.getByTestId('migrate-error').textContent).toContain('network down')
  })

  it('打开时后端已有在跑的任务 → 直接进进度态（关过对话框不丢进度），并继续轮询到完成', async () => {
    calls.assets.mockResolvedValue({ items: [asset(1), asset(2)], total: 2 })
    let n = 0
    calls.migrateToLibraryStatus.mockImplementation(async (batchId?: string) => {
      if (!batchId) return { state: 'running', batchId: 'resumed', total: 2, processed: 1, migrated: 1, skipped: 0, failed: 0, bytes: 0 }
      n += 1
      if (n === 1) return { state: 'running', batchId, total: 2, processed: 1, migrated: 1, skipped: 0, failed: 0, bytes: 0 }
      return {
        state: 'done',
        batchId,
        total: 2,
        processed: 2,
        migrated: 2,
        skipped: 0,
        failed: 0,
        bytes: 4096,
        report: { batchId, total: 2, migrated: 2, skipped: [], failed: [], bytes: 4096, warnings: [] },
      }
    })
    await renderDialog('/gallery?dialog=migrate-to-library&ids=1,2')
    await waitFor(() => expect(screen.getByTestId('migrate-progress')).toBeTruthy())
    // 恢复态下「开始迁移」不该可点（同一时刻只许一批）
    expect((screen.getByTestId('migrate-start') as HTMLButtonElement).disabled).toBe(true)
    await waitFor(() => expect(screen.getByTestId('migrate-report')).toBeTruthy(), { timeout: 3000 })
    expect(screen.getByTestId('migrate-report').textContent).toContain('成功 2')
  })
})

// ───────────────────────── 图库工具条入口 ─────────────────────────

describe('DT7 图库工具条入口：禁用态 + URL 直达', () => {
  it('无选择集 → 批量动作条不渲染动作按钮，改为提示「先选中图片」（KI-16 #5 SelectionBar 契约）', async () => {
    calls.assets.mockResolvedValue({ items: [asset(1)], total: 1 })
    await renderGallery('/gallery')
    // KI-16 #5：批量动作（含迁入库）只在有选择集时渲染（共用 SelectionBar 的 actions 语义）；
    // 「不可用但灰置 + 原因」的 DT7 口径在有选择集时继续成立（下面三条用例覆盖）
    await waitFor(() => expect(screen.getByTestId('selection-bar')).toBeTruthy())
    expect(screen.queryByTestId('migrate-button')).toBeNull()
    expect(screen.getByTestId('gallery-batch-hint').textContent).toContain('先选中图片')
    expect(screen.getByTestId('selection-select-all').textContent).toContain('全选已加载 1 张')
  })

  it('没有可写库根 → 禁用 + 「库根」原因', async () => {
    __setCapabilitiesForTest({ ...capsWithRoot, writableRoots: [], roots: [{ path: '/data', writable: false }] })
    calls.assets.mockResolvedValue({ items: [asset(1)], total: 1 })
    await renderGallery('/gallery?ids=1')
    await waitFor(() => expect(screen.getByTestId('migrate-button')).toBeTruthy())
    const btn = screen.getByTestId('migrate-button') as HTMLButtonElement
    expect(btn.disabled).toBe(true)
    expect(btn.title).toContain('可写的库根')
  })

  it('选中项全是 copyin → 禁用 + 「无需迁移」原因', async () => {
    calls.assets.mockResolvedValue({ items: [asset(1, { storeMode: 'copyin' })], total: 1 })
    await renderGallery('/gallery?ids=1')
    await waitFor(() => expect(screen.getByTestId('migrate-button')).toBeTruthy())
    // 资产读回前按钮不拦（未知 → 允许）；读回后按「全不可迁」灰置
    await waitFor(() =>
      expect((screen.getByTestId('migrate-button') as HTMLButtonElement).disabled).toBe(true),
    )
    expect((screen.getByTestId('migrate-button') as HTMLButtonElement).title).toContain('无需迁移')
  })

  it('有可迁项 → 可点；点开把选择集写进 URL 并渲染对话框', async () => {
    calls.assets.mockResolvedValue({ items: [asset(1)], total: 1 })
    await renderGallery('/gallery?ids=1')
    await waitFor(() => expect(screen.getByTestId('migrate-button')).toBeTruthy())
    const btn = screen.getByTestId('migrate-button') as HTMLButtonElement
    expect(btn.disabled).toBe(false)

    fireEvent.click(btn)
    await waitFor(() =>
      expect(screen.getByTestId('loc').textContent).toContain('dialog=migrate-to-library'),
    )
    expect(screen.getByTestId('loc').textContent).toContain('ids=1')
    await waitFor(() => expect(screen.getByTestId('migrate-dialog-title')).toBeTruthy())
    expect(screen.getByTestId('migrate-dialog-title').textContent).toBe('迁入库（1 张）')
  })
})
