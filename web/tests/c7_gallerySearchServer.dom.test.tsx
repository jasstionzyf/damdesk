// @vitest-environment jsdom
// @vitest-environment-options {"url": "http://localhost:3000/gallery"}
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { BrowserRouter } from 'react-router-dom'
import { ToastProvider } from '../src/lib/toast'
import { ConfirmProvider } from '../src/lib/confirm'

/**
 * C7（卡 t_eadc13a3）：图库关键词 = **服务端全库检索** + 「输完才查」。
 *
 * 用户实测：「搜索输入框还是不行啊，输入文字查询还是有问题，来不及输全就搜索」。
 * 两条症状 → 两条断言族：
 *
 *  ①「来不及输全就搜」：静默窗口 300ms 太短（中文逐字/逐词输入照样在中间态就查），
 *     现在 800ms；急的用户按 Enter 立即查（不等窗口）；
 *  ②「结果不全」：关键词曾是**客户端** `filterByQuery`，只筛已加载的那一页（PAGE_SIZE 60），
 *     全库命中的图在首页之后永远搜不到 → 现在 `?q=` 进 `GET /api/library/assets`，
 *     命中数与「命中 N 张」都取服务端 `total`。
 *
 * 断言口（jsdom 里 VirtuosoGrid 无视口高度、不渲染卡片，故不用卡片数）：
 *  - `apiStub.assets.mock.calls`：**请求才是检索发生的证据**（客户端筛选时代这里恒定不变）；
 *  - `filter-count`：`已载 L / 命中 T`，关键词生效后的可见证据；
 *  - `没有匹配` / `gallery-search-pending`：在途期间不许闪空态；
 *  - URL 的 `q`：深链 / 刷新 / back 语义不变。
 *
 * 时间：静默窗口 800ms，用例里等 900ms 过窗口（真实计时器，与其它 dom 用例同风格）。
 */

const store = new Map<string, string>()
Object.defineProperty(globalThis, 'localStorage', {
  value: {
    getItem: (k: string) => store.get(k) ?? null,
    setItem: (k: string, v: string) => void store.set(k, String(v)),
    removeItem: (k: string) => void store.delete(k),
    clear: () => store.clear(),
    key: (i: number) => [...store.keys()][i] ?? null,
    get length() {
      return store.size
    },
  } satisfies Storage,
  configurable: true,
  writable: true,
})

/** 三张图：'sno' / '雪' 命中 2 张，'zzz' 0 张 */
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
  }
}

const apiStub = vi.hoisted(() => ({
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
  thumbUrl: (id: number, size = 256) => `/api/media/thumb/${id}?size=${size}`,
  originalUrl: (id: number) => `/api/media/original/${id}`,
}))

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
apiStub.capabilities.mockImplementation(async () => ({ features: {} }))
apiStub.assetTags.mockImplementation(async () => [])
apiStub.exportStatus.mockImplementation(async () => ({ running: false, progress: null, report: null, error: null }))
apiStub.appInfo.mockImplementation(async () => ({ dataDir: '/data' }))
apiStub.cameras.mockImplementation(async () => ({ cameras: [] }))

/**
 * 服务端语义的桩：`q` 按文件名 / 标题包含匹配（大小写不敏感），无 `q` = 全量。
 * 桩必须自己实现过滤——否则「命中 2 张」在客户端筛选时代也成立，用例证明不了服务端检索。
 */
async function defaultAssets(q: { q?: string } = {}) {
  const needle = (q.q ?? '').trim().toLowerCase()
  const hit = needle
    ? ASSETS.filter((a) =>
        [a.fileName, a.titleCh, `/library/${a.fileName}`].some((s) =>
          s.toLowerCase().includes(needle),
        ),
      )
    : ASSETS
  return { total: hit.length, items: hit.map(assetOf) }
}

// 视口按「三栏放得下」的桌面宽度（jsdom 默认 1024 → 侧栏转抽屉，抽屉会用 capture 相位
// 吃掉 Esc，测不到「输入框内 Esc 清空」这条）。1920 宽下三栏常驻，Esc 归输入框。
Object.defineProperty(window, 'innerWidth', { value: 1920, writable: true, configurable: true })
Object.defineProperty(window, 'innerHeight', { value: 1080, writable: true, configurable: true })

// 三栏布局靠这两条浏览器能力：jsdom 都没有，给最小桩（否则 Gallery 挂载即崩）
Object.defineProperty(window, 'matchMedia', {
  writable: true,
  configurable: true,
  value: (q: string) => ({
    matches: /min-width:\s*1280px/.test(q),
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

function renderGallery(path = '/gallery') {
  // 用真 BrowserRouter（+ history.replaceState 设初值）：URL 是唯一真源，
  // 这里断言的就是浏览器地址栏里的 ?q=，MemoryRouter 碰不到 window.location。
  window.history.replaceState(null, '', path)
  return render(
    <BrowserRouter>
      <ToastProvider>
        <ConfirmProvider>
          <Gallery onImport={() => {}} />
        </ConfirmProvider>
      </ToastProvider>
    </BrowserRouter>,
  )
}

const sleep = (ms: number) => act(async () => void (await new Promise((r) => setTimeout(r, ms))))
const input = () => screen.getByTestId('library-search') as HTMLInputElement
/** 「已载 L / 命中 T」只在关键词非空时出现 → appliedQuery 已生效的可见证据 */
const count = () => screen.queryByTestId('filter-count')?.textContent ?? null
const zeroHint = () => document.body.textContent?.includes('没有匹配') ?? false
const pendingHint = () => screen.queryByTestId('gallery-search-pending') !== null
/** 模拟真实输入法的组合期 input 事件（Chromium 会带 isComposing=true；fireEvent.change 不带） */
function composingChange(el: HTMLInputElement, value: string) {
  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')?.set
  setter?.call(el, value)
  el.dispatchEvent(new InputEvent('input', { bubbles: true, isComposing: true, data: value }))
}
const qOfUrl = () => new URLSearchParams(window.location.search).get('q')
const assetCalls = () => apiStub.assets.mock.calls.length
/** 最近一次 assets 请求里的关键词（服务端检索的铁证） */
const lastSentQ = () => (apiStub.assets.mock.calls.at(-1)?.[0] as { q?: string } | undefined)?.q

async function settle() {
  await waitFor(() => expect(document.body.textContent).toContain(`已加载 ${ASSETS.length}`))
}

describe('C7 图库搜索：服务端全库检索 + 输完才查（卡 t_eadc13a3）', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    store.clear()
    window.history.replaceState(null, '', '/gallery')
    // 每个用例都从「服务端语义桩」起步（个别用例会临时换掉实现，这里复位）
    apiStub.assets.mockImplementation(defaultAssets)
  })
  afterEach(() => cleanup())

  it('连敲 3 字符：静默窗口内 0 请求，过窗口后只按最终串查一次（中间串不进请求）', async () => {
    renderGallery()
    await settle()
    const afterMount = assetCalls()
    expect(afterMount).toBeGreaterThan(0)

    fireEvent.change(input(), { target: { value: 's' } })
    fireEvent.change(input(), { target: { value: 'sn' } })
    fireEvent.change(input(), { target: { value: 'sno' } })

    // 输入框立刻有字（受控值每次 input 都同步 = 打字一定看得见）
    expect(input().value).toBe('sno')
    // 但 URL 还没提交：没敲完的串不落进深链（提交后才写，见下）
    expect(qOfUrl()).toBeNull()
    // 静默窗口内：没有请求、没有「已载 / 命中」、没有空态、上一份结果还在
    expect(assetCalls()).toBe(afterMount)
    expect(count()).toBeNull()
    expect(zeroHint()).toBe(false)
    expect(document.body.textContent).toContain(`已加载 ${ASSETS.length}`)

    // 窗口 800ms：700ms 时仍然静默（旧实现 300ms 此时早就查了）
    await sleep(700)
    expect(assetCalls()).toBe(afterMount)

    await sleep(250)
    // 过窗口 → 只发 1 次请求，且只有最终串 'sno'（'s'、'sn' 从未进请求）
    expect(assetCalls()).toBe(afterMount + 1)
    expect(lastSentQ()).toBe('sno')
    await waitFor(() => expect(count()).toBe('已载 2 / 命中 2'))
    expect(input().value).toBe('sno')
    expect(qOfUrl()).toBe('sno') // 提交后 URL 才是唯一真源（深链 / 刷新 / back 语义不变）
  })

  it('IME 组合期挂起：拼音不进 URL、不进请求；compositionEnd 才按最终中文查', async () => {
    renderGallery()
    await settle()
    const before = assetCalls()
    const el = input()

    fireEvent.compositionStart(el)
    // 拼音阶段：输入法把 'x' 'xue' 'xueshan' 一路打进输入框，全都是「还没成字」的中间态
    // （真实 Chromium 里这些 input 事件带 isComposing=true，jsdom 必须手动带上，否则等价于普通输入）
    composingChange(el, 'x')
    composingChange(el, 'xue')
    composingChange(el, 'xueshan')

    // K07-FIX-K：组合期**必须看得见字**（旧实现在 onChange 里 return，React 下一次渲染
    // 把 DOM 值重置回旧串 → 真机中文输入法「打不出字」）
    expect(input().value).toBe('xueshan')
    expect(qOfUrl()).toBeNull()
    expect(count()).toBeNull()
    expect(zeroHint()).toBe(false)
    await sleep(900) // 即使过了静默窗口也不许查
    expect(assetCalls()).toBe(before)

    // 组合结束（选定「雪」）→ 只落最终值，过窗口后只查一次 '雪'
    fireEvent.compositionEnd(el, { target: { value: '雪' } })
    expect(input().value).toBe('雪')
    await sleep(900)
    expect(qOfUrl()).toBe('雪') // 提交（过静默窗口）后 URL 才落地
    expect(assetCalls()).toBe(before + 1)
    expect(lastSentQ()).toBe('雪')
    await waitFor(() => expect(count()).toBe('已载 2 / 命中 2'))
  })

  it('compositionEnd 缺失（切窗口 / 输入法取消）也不能锁死输入框：字照显、后续输入照常生效', async () => {
    renderGallery()
    await settle()
    const before = assetCalls()
    const el = input()

    // 组合开始 → 拼音输入；但 compositionend 永不到达（旧实现的 ref 闩锁在这里永久为 true）
    fireEvent.compositionStart(el)
    composingChange(el, 'xue')
    expect(el.value).toBe('xue')

    // IME 直接把「雪」提交成一次普通 input（isComposing=false）
    fireEvent.change(el, { target: { value: '雪' } })
    await sleep(900)
    expect(assetCalls()).toBe(before + 1)
    expect(lastSentQ()).toBe('雪')
    expect(input().value).toBe('雪')

    // 再普通输入一次：闩锁若残留，这里会被吞掉（不显示、不查）
    // 注意：提交会让 loadSig 变 → AssetGrid 整块重建（搜索框 DOM 换新），必须重新取节点，
    // 否则是对已卸载节点派发事件（页面这边则由「焦点归还」effect 兜住用户体感）。
    fireEvent.change(input(), { target: { value: '雪x' } })
    expect(input().value).toBe('雪x')
    await sleep(900)
    expect(assetCalls()).toBe(before + 2)
    expect(lastSentQ()).toBe('雪x')
  })

  it('零命中时搜索框必须还在：用户能继续改关键词（旧实现整条工具条被空态顶掉 = 输入框消失）', async () => {
    renderGallery()
    await settle()
    const el = input()
    el.focus()

    composingChange(el, 'zzzzzz') // 命中 0 的关键词
    fireEvent.keyDown(el, { key: 'Enter' })
    await waitFor(() => expect(zeroHint()).toBe(true))

    // 输入框仍在、而且是**同一个 DOM 节点**（工具条不卸载 = 焦点/光标不会丢）
    const still = input()
    expect(still).toBe(el)
    expect(still.isConnected).toBe(true)
    fireEvent.change(still, { target: { value: 'sno' } })
    expect((screen.getByTestId('library-search') as HTMLInputElement).value).toBe('sno')
    fireEvent.keyDown(screen.getByTestId('library-search'), { key: 'Enter' })
    await waitFor(() => expect(count()).toBe('已载 2 / 命中 2'))
  })

  it('Enter 立即查：不等 800ms 静默窗口，只发一次', async () => {
    renderGallery()
    await settle()
    const before = assetCalls()

    fireEvent.change(input(), { target: { value: 'sno' } })
    expect(assetCalls()).toBe(before) // 静默期内还没查
    fireEvent.keyDown(input(), { key: 'Enter' })

    // 不等窗口就落地
    await waitFor(() => expect(count()).toBe('已载 2 / 命中 2'))
    expect(assetCalls()).toBe(before + 1)
    expect(lastSentQ()).toBe('sno')

    // Enter 之后静默计时器不许再补一枪
    await sleep(900)
    expect(assetCalls()).toBe(before + 1)
  })

  it('零命中：请求在途不出空态，落地后才出「全库检索没有命中」', async () => {
    renderGallery()
    await settle()
    // 挂载请求已完成，从这里开始把 assets 换成「可控的 0 命中」响应
    let release: (() => void) | null = null
    apiStub.assets.mockImplementation(
      async () =>
        new Promise((resolve) => {
          release = () => resolve({ total: 0, items: [] })
        }),
    )

    fireEvent.change(input(), { target: { value: 'zzz' } })
    await sleep(900) // 请求已发、未回

    // 在途：沿用上一份结果，不出「没有匹配」也不算 0 张
    expect(zeroHint()).toBe(false)
    expect(document.body.textContent).toContain(`已加载 ${ASSETS.length}`)

    await act(async () => {
      release!()
    })
    await waitFor(() => expect(zeroHint()).toBe(true))
    expect(document.body.textContent).toContain('全库检索（文件名 / 标题 / 描述 / 路径）没有命中')
    expect(assetCalls()).toBeGreaterThan(0)
  })

  it('清空立即生效（不 debounce）：输入框内 Esc → 同一帧回全量', async () => {
    renderGallery()
    await settle()

    fireEvent.change(input(), { target: { value: 'sno' } })
    await sleep(900)
    await waitFor(() => expect(count()).toBe('已载 2 / 命中 2'))

    const before = assetCalls()
    fireEvent.keyDown(input(), { key: 'Escape' })
    // 不等 800ms：清空是「用户明确的动作」，不许延迟
    expect(qOfUrl()).toBeNull()
    await waitFor(() => expect(count()).toBeNull())
    await waitFor(() => expect(document.body.textContent).toContain(`已加载 ${ASSETS.length}`))
    expect(assetCalls()).toBe(before + 1)
    expect(lastSentQ()).toBeUndefined()
  })

  it('「命中 N 张」取服务端 total（不是已加载条数）', async () => {
    // 服务端说全库命中 5 张，这一页只回来 3 张 —— 文案必须写 5，否则用户以为丢图
    apiStub.assets.mockImplementationOnce(async () => ({ total: 5, items: ASSETS.map(assetOf) }))
    renderGallery('/gallery?q=sno')
    await waitFor(() => expect(count()).toBe('已载 3 / 命中 5'))
    // 选择条同时提示「仅作用于已加载的 3 张（共 5 张）」
    expect(document.body.textContent).toContain('仅作用于已加载的 3 张（共 5 张）')
  })

  it('深链 ?q= 恢复：首屏首个请求就带 q（服务端检索，不是客户端过滤）', async () => {
    renderGallery('/gallery?q=%E9%9B%AA')
    await waitFor(() => expect(count()).toBe('已载 2 / 命中 2'))
    expect(input().value).toBe('雪')
    expect(lastSentQ()).toBe('雪')
  })

  it('本地偏好兜底（URL 缺键）时，横幅命中数同样取服务端 total', async () => {
    store.set('dam.gallery.query', 'sno')
    renderGallery()
    await waitFor(() =>
      expect(screen.getByTestId('pref-filter-hits').textContent).toContain('命中 2 张'),
    )
    expect(screen.getByTestId('pref-filter-banner').textContent).toContain('关键词「sno」')
    expect(input().value).toBe('sno')
    expect(lastSentQ()).toBe('sno')
  })
})
