// @vitest-environment jsdom
// @vitest-environment-options {"url": "http://localhost:3000/"}
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { readFileSync, readdirSync, statSync, existsSync } from 'node:fs'
import { resolve } from 'node:path'
import { BrowserRouter, Route, Routes } from 'react-router-dom'
import { ToastProvider } from '../src/lib/toast'
import { ConfirmProvider } from '../src/lib/confirm'

// i18n-C（卡 task）：本文件断言的是**中文界面原文**，而迁移后文案走 t()、
// jsdom 的 navigator.language 默认 en-US（会渲染英文）。这里把语言钉成 zh-Hans ——
// t() 在中文下缺译回退 key（= 中文原文），断言逐字不变、强度不降，只是显式声明
// 「本文件验中文文案」。vi.hoisted 早于所有 import 执行，故早于 i18n 模块初始化。
vi.hoisted(() => {
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
 * KI-16 #7（卡 task）：智能文件夹**并入图库**，独立页面删除。
 *
 * 判据（卡体验收项）：
 *  1. `web/src` 里不再有 SmartFolderPage（页面文件已删，`/smart/:id` 只剩重定向）；
 *  2. `/smart/7` → 跳到 `/?dir=…&tags=…&sf=7`，图库按保存条件过滤且横幅显示文件夹名；
 *  3. 侧栏条目点击 = 同一组条件进图库；右键菜单可重命名 / 删除，删除后深链给「文件夹不存在」；
 *  4. 计数对账：与保存时记下的 total 不等 → 芯片标「条件已变」（相等则安静）。
 *
 * 说明：图库的 `?sf=` 只是「来源」标记，条件本身落在 dir/tags/untagged/missing 上
 * （与侧栏/目录树/芯片同一套 URL 契约，见 lib/galleryUrlState.ts）。
 */

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

const local = memStorage()
const session = memStorage()
Object.defineProperty(globalThis, 'localStorage', { value: local, configurable: true, writable: true })
Object.defineProperty(globalThis, 'sessionStorage', { value: session, configurable: true, writable: true })
// i18n-C（卡 task）：本文件的组件是**动态 import**（下面 line ~225），i18n 模块在那时才
// 初始化并读 localStorage 定初值 —— 所以语言必须在动态 import 之前就写进这个内存 stub。
local.setItem('dam.locale', 'zh-Hans')

const SRC = resolve(__dirname, '../src')

const ASSETS = [
  { assetId: 11, fileName: 'snow-1.jpg', titleCh: '雪山日出' },
  { assetId: 12, fileName: 'snow-2.jpg', titleCh: '雪山云海' },
  { assetId: 13, fileName: 'beach.jpg', titleCh: '海滩' },
]

function assetOf(a: (typeof ASSETS)[number]) {
  return {
    assetId: a.assetId,
    mediaType: 'image',
    absPath: `/library/${a.fileName}`,
    storeMode: 'index',
    missing: 0,
    fileName: a.fileName,
    fileExt: 'jpg',
    fileSz: 2048,
    mtime: 0,
    fileHash: null,
    titleCh: a.titleCh,
    titleEn: null,
    description: null,
    source: 'index',
    parentAssetId: null,
    searchable: 1,
    deleted: 0,
    approvalStatus: 'pending',
    starRating: 0,
    colorLabel: null,
    cloudAssetId: null,
    importedAt: 0,
    embStatus: 'done',
    embModel: 'stub',
    dedupStatus: 'none',
    dedupModel: null,
    thumbPath: null,
    width: 1200,
    height: 800,
    pHash: null,
  }
}

/** 侧栏里那一个文件夹（条件：目录 /lib/a + 标签 雪山） */
const FOLDER = {
  id: 7,
  name: '精选雪山',
  query: JSON.stringify({ tag: '雪山', dirPrefix: '/lib/a' }),
  createdAt: 0,
  updatedAt: 0,
}

const apiStub = {
  stats: vi.fn(),
  dirs: vi.fn(),
  tags: vi.fn(),
  complianceStatus: vi.fn(),
  importStatus: vi.fn(),
  assets: vi.fn(),
  capabilities: vi.fn(),
  assetTags: vi.fn(),
  exportStatus: vi.fn(),
  appInfo: vi.fn(),
  cameras: vi.fn(),
  complianceVerdicts: vi.fn(),
  complianceReview: vi.fn(),
  complianceReviewCreate: vi.fn(),
  complianceReviewRevoke: vi.fn(),
  ocrAssetText: vi.fn(),
  smartFolders: vi.fn(),
  smartFolder: vi.fn(),
  smartFolderCreate: vi.fn(),
  smartFolderUpdate: vi.fn(),
  smartFolderDelete: vi.fn(),
  thumbUrl: (id: number, size = 256) => `/api/media/thumb/${id}?size=${size}`,
  originalUrl: (id: number) => `/api/media/original/${id}`,
}

vi.mock('../src/lib/api', () => ({
  api: apiStub,
  formatSize: (n: number) => `${n} B`,
  ApiError: class ApiError extends Error {
    code?: string
    status = 500
    constructor(msg: string, status = 500, code?: string) {
      super(msg)
      this.code = code
      this.status = status
    }
  },
}))

apiStub.stats.mockImplementation(async () => ({
  assets: ASSETS.length,
  missing: 0,
  tagged: 0,
  totalBytes: 0,
  byMediaType: [{ mediaType: 'image', count: ASSETS.length }],
  missingCheckedAt: null,
  missingStale: false,
  missingUnreadable: 0,
  broken: 0,
  missingStaleSecs: 86400,
}))
apiStub.dirs.mockImplementation(async () => ({ dirs: [], roots: [] }))
apiStub.tags.mockImplementation(async () => [])
apiStub.complianceStatus.mockImplementation(async () => ({
  states: { pass: 0, fail: 0, unknown: 0, pending: 0, exempt: 0 },
  pack: { id: 'stub' },
}))
apiStub.importStatus.mockImplementation(async () => ({ running: false, progress: null }))
apiStub.assets.mockImplementation(async () => ({ total: ASSETS.length, items: ASSETS.map(assetOf) }))
apiStub.capabilities.mockImplementation(async () => ({ features: {} }))
apiStub.assetTags.mockImplementation(async () => [])
apiStub.exportStatus.mockImplementation(async () => ({ running: false, progress: null, report: null, error: null }))
apiStub.appInfo.mockImplementation(async () => ({ dataDir: '/data' }))
apiStub.cameras.mockImplementation(async () => ({ cameras: [] }))
apiStub.complianceVerdicts.mockImplementation(async (assetId: number) => ({
  packId: 'stub',
  assetId,
  state: 'unknown',
  stateLabel: '未判定',
  machineState: 'unknown',
  machineStateLabel: '未判定',
  machineConfidence: null,
  assetHumanReview: null,
  items: [],
}))
apiStub.ocrAssetText.mockImplementation(async (assetId: number) => ({ assetId, status: 'none', text: null, blocks: [] }))
apiStub.complianceReview.mockImplementation(async () => ({ items: [], total: 0, nextCursor: null }))
apiStub.smartFolders.mockImplementation(async () => [FOLDER])
apiStub.smartFolder.mockImplementation(async (id: number) => {
  if (id !== FOLDER.id) throw new Error('智能文件夹不存在')
  return FOLDER
})
apiStub.smartFolderDelete.mockImplementation(async () => ({ ok: true }))

Object.defineProperty(window, 'innerWidth', { value: 1440, writable: true, configurable: true })
Object.defineProperty(window, 'innerHeight', { value: 900, writable: true, configurable: true })
Object.defineProperty(window, 'matchMedia', {
  writable: true,
  configurable: true,
  value: (q: string) => ({
    matches: true,
    media: q,
    onchange: null,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    dispatchEvent: () => false,
  }),
})
;(globalThis as unknown as { ResizeObserver: unknown }).ResizeObserver = class {
  observe() {}
  unobserve() {}
  disconnect() {}
}

const { Gallery } = await import('../src/pages/Gallery')
const { SmartFolderRedirect } = await import('../src/pages/SmartFolderRedirect')
const { SmartFolderSection } = await import('../src/components/SmartFolderSection')
const { recordFolderCount } = await import('../src/lib/smartFolders')

function renderApp(path: string, withSidebar = false) {
  window.history.replaceState(null, '', path)
  return render(
    <BrowserRouter>
      <ToastProvider>
        <ConfirmProvider>
          {withSidebar && <SmartFolderSection />}
          <Routes>
            <Route path="/" element={<Gallery onImport={() => {}} />} />
            <Route path="/smart/:id" element={<SmartFolderRedirect />} />
          </Routes>
        </ConfirmProvider>
      </ToastProvider>
    </BrowserRouter>,
  )
}

const search = () => new URLSearchParams(window.location.search)
const banner = () => screen.queryByTestId('filter-banner')?.textContent ?? ''
async function settle() {
  await waitFor(() => expect(document.body.textContent).toContain(`已加载 ${ASSETS.length}`))
}

beforeEach(() => {
  vi.clearAllMocks()
  localStorage.clear()
  sessionStorage.clear()
  window.history.replaceState(null, '', '/')
  // i18n-C（卡 task）：本文件断言中文界面原文；清完 storage 后语言会回落
  // 到 navigator（en-US）→ 渲染英文。这里把语言补回 zh-Hans（t() 缺译回退中文原文）。
  localStorage.setItem('dam.locale', 'zh-Hans')
})
afterEach(() => cleanup())

describe('KI-16 #7：智能文件夹并入图库（静态棘轮 R45）', () => {
  it('web/src 里不再有 SmartFolderPage（页面已删，只剩重定向实现）', () => {
    expect(existsSync(resolve(SRC, 'pages/SmartFolderPage.tsx'))).toBe(false)
    const walk = (dir: string): string[] =>
      readdirSync(dir).flatMap((n) => {
        const p = resolve(dir, n)
        return statSync(p).isDirectory() ? walk(p) : [p]
      })
    const hits = walk(SRC).filter((f) => /\.tsx?$/.test(f) && readFileSync(f, 'utf8').includes('SmartFolderPage'))
    expect(hits).toEqual([])
  })
})

describe('KI-16 #7：`/smart/:id` 重定向到图库条件', () => {
  it('/smart/7 → /?dir=/lib/a&tags=雪山&sf=7，图库按条件过滤且横幅显示文件夹名', async () => {
    renderApp('/smart/7')
    await waitFor(() => expect(window.location.pathname).toBe('/'))
    await settle()

    expect(window.location.pathname).toBe('/')
    const q = search()
    expect(q.get('sf')).toBe('7')
    expect(q.get('dir')).toBe('/lib/a')
    expect(q.get('tags')).toBe('雪山')

    // 服务端查询带上保存的条件（和图库自身筛选用同一组参数名）
    const call = apiStub.assets.mock.calls.at(-1)?.[0] as Record<string, unknown>
    expect(call.dirPrefix).toBe('/lib/a')
    expect(call.tag).toBe('雪山')

    // 横幅：来源芯片 + 条件芯片
    const text = banner()
    expect(text).toContain('智能文件夹：精选雪山')
    expect(text).toContain('目录 a')
    expect(text).toContain('标签 雪山')
  })

  it('老深链指向已删除的文件夹 → 「智能文件夹不存在」可读提示（不 404、不白屏）', async () => {
    renderApp('/smart/99')
    await waitFor(() => expect(screen.queryByTestId('smart-redirect-error')).not.toBeNull())
    expect(document.body.textContent).toContain('智能文件夹不存在')
    expect(document.body.textContent).toContain('返回图库')
  })
})

describe('KI-16 #7：侧栏条目 = 图库 + 保存的条件', () => {
  it('点侧栏条目 → 图库 URL 带上条件（不再是 /smart/:id）', async () => {
    renderApp('/', true)
    const row = await screen.findByTestId('sf-row-7')
    const item = row.querySelector('button')!
    fireEvent.click(item)

    await settle()
    expect(window.location.pathname).toBe('/')
    const q = search()
    expect(q.get('sf')).toBe('7')
    expect(q.get('tags')).toBe('雪山')
    expect(q.get('dir')).toBe('/lib/a')
    expect(banner()).toContain('智能文件夹：精选雪山')
  })

  it('右键菜单：重命名打开编辑对话框、删除调后端', async () => {
    renderApp('/', true)
    const row = await screen.findByTestId('sf-row-7')
    fireEvent.contextMenu(row)
    const menu = await screen.findByTestId('sf-context-menu')
    expect(menu.textContent).toContain('重命名')
    expect(menu.textContent).toContain('删除')

    // 重命名 → 编辑对话框（标题带「编辑智能文件夹」）
    fireEvent.click(screen.getByTestId('sf-menu-rename'))
    await waitFor(() => expect(document.body.textContent).toContain('编辑智能文件夹'))

    // 再右键 → 删除
    fireEvent.contextMenu(screen.getByTestId('sf-row-7'))
    fireEvent.click(await screen.findByTestId('sf-menu-delete'))
    await waitFor(() => expect(apiStub.smartFolderDelete).toHaveBeenCalledWith(7))
  })
})

describe('KI-16 #7：计数对账（保存时 count vs 应用后 total）', () => {
  it('相等 → 芯片不标「条件已变」（两个数字都是 3）', async () => {
    recordFolderCount(7, ASSETS.length)
    renderApp('/?sf=7&dir=/lib/a&tags=雪山')
    await settle()
    expect(banner()).toContain('智能文件夹：精选雪山')
    expect(banner()).not.toContain('条件已变')
  })

  it('不等 → 芯片标「条件已变」并给出两个数字（保存时 5，现 3）', async () => {
    recordFolderCount(7, 5)
    renderApp('/?sf=7&dir=/lib/a&tags=雪山')
    await settle()
    await waitFor(() => expect(banner()).toContain('条件已变'))
    expect(banner()).toContain('保存时 5 张')
    expect(banner()).toContain('现 3 张')
  })

  it('芯片 × 只解除智能文件夹绑定，条件芯片保留', async () => {
    renderApp('/?sf=7&dir=/lib/a&tags=雪山')
    await settle()
    await waitFor(() => expect(screen.queryByTestId('chip-remove-sf')).not.toBeNull())
    fireEvent.click(screen.getByTestId('chip-remove-sf'))
    await waitFor(() => expect(search().get('sf')).toBeNull())
    expect(search().get('tags')).toBe('雪山')
    expect(search().get('dir')).toBe('/lib/a')
    expect(banner()).not.toContain('智能文件夹：')
  })

  it('文件夹被删（后端 404）→ 芯片写明「文件夹不存在」，已带条件照旧生效', async () => {
    renderApp('/?sf=9&tags=雪山')
    await settle()
    await waitFor(() => expect(banner()).toContain('文件夹不存在'))
    const call = apiStub.assets.mock.calls.at(-1)?.[0] as Record<string, unknown>
    expect(call.tag).toBe('雪山')
  })
})
