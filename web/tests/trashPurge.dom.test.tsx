// @vitest-environment jsdom
// @vitest-environment-options {"url": "http://localhost:3000/"}
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter, Route, Routes } from 'react-router-dom'
import { ToastProvider } from '../src/lib/toast'
import { ConfirmProvider } from '../src/lib/confirm'
import { __setCapabilitiesForTest } from '../src/lib/caps'
import { Trash } from '../src/pages/Trash'
import type { AssetView, Capabilities } from '../src/lib/api'

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
 * KI-11（卡 t_aef170e1）验收：回收站「彻底删除」= **先确认后执行** + 能力位门控。
 *
 * 三条硬要求（用户原话：点击后先弹确认框，确认才执行删除）：
 *  1. 能力位 `features.physicalDelete=false` → 按钮灰置 + 写明原因（不假装能删）；
 *  2. 能力位 true + 选中 1 张 → 点按钮**只弹确认框**，此刻后端接口一次都没被调用；
 *  3. 确认 → 调 `dedupPurge` 且刷新列表；取消 → 零请求、列表不变。
 *
 * 真组件树（Trash + ToastProvider + ConfirmProvider），只把后端 api 换成桩。
 */
const apiStub = vi.hoisted(() => ({
  dedupTrash: vi.fn(),
  dedupPurge: vi.fn(),
  dedupRestore: vi.fn(),
  capabilities: vi.fn(),
  thumbUrl: (id: number, size: 256 | 1024 = 256) => `/api/media/thumb/${id}?size=${size}`,
}))

vi.mock('../src/lib/api', () => ({
  api: apiStub,
  formatSize: (n: number) => `${(n / 1024).toFixed(1)} KB`,
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

function assetOf(id: number, storeMode: 'copyin' | 'reference' = 'reference'): AssetView {
  return {
    assetId: id,
    mediaType: 'image',
    absPath: `/corpus/a${id}.jpg`,
    storeMode,
    missing: 0,
    fileName: `a${id}.jpg`,
    fileExt: 'jpg',
    fileSz: 2048,
    mtime: 0,
    fileHash: null,
    titleCh: null,
    titleEn: null,
    description: null,
    source: 'userUpload',
    parentAssetId: null,
    searchable: 1,
    deleted: 1,
    approvalStatus: 'approved',
    starRating: 0,
    colorLabel: null,
    cloudAssetId: null,
    importedAt: 0,
    embStatus: 'done',
    embModel: 'stub',
    dedupStatus: 'none',
    dedupModel: null,
    thumbPath: null,
    width: 100,
    height: 100,
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

const TRASH = [assetOf(11, 'copyin'), assetOf(12, 'reference')]

function capsWith(physicalDelete: boolean, reason = ''): Capabilities {
  return {
    isDesktop: physicalDelete,
    storeMode: physicalDelete ? 'local-disk' : 'server-volume',
    writableRoots: physicalDelete ? ['/corpus'] : [],
    roots: [],
    features: {
      moveAsset: physicalDelete,
      renameAsset: physicalDelete,
      relink: physicalDelete,
      openInOS: physicalDelete,
      physicalDelete,
      copyInImport: true,
    },
    featureReasons: physicalDelete ? {} : { physicalDelete: reason },
  } as Capabilities
}

function renderTrash() {
  return render(
    <ToastProvider>
      <ConfirmProvider>
        <MemoryRouter initialEntries={['/trash']}>
          <Routes>
            <Route path="/trash" element={<Trash />} />
          </Routes>
        </MemoryRouter>
      </ConfirmProvider>
    </ToastProvider>,
  )
}

beforeEach(() => {
  apiStub.dedupTrash.mockImplementation(async () => ({ items: TRASH }))
  apiStub.dedupPurge.mockImplementation(async (ids: number[]) => ({
    ok: true,
    purged: ids.length,
    skipped: 0,
    purgedIds: ids,
    skippedIds: [],
    rows: ids.length * 4,
    vectors: ids.length,
    thumbs: ids.length * 2,
    filesDeleted: ids.length * 3,
    freedBytes: 3072,
  }))
  apiStub.dedupRestore.mockImplementation(async () => ({ ok: true, restored: 1 }))
  __setCapabilitiesForTest(capsWith(true))
})

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
  __setCapabilitiesForTest(null)
})

async function selectFirst() {
  const box = (await screen.findByLabelText('选择 a11.jpg')) as HTMLInputElement
  fireEvent.click(box)
  return box
}

describe('KI-11 回收站彻底删除', () => {
  it('能力位 physicalDelete=false → 按钮灰置并写明原因（不假装能删）', async () => {
    __setCapabilitiesForTest(capsWith(false, '网页版不支持物理删除磁盘文件；请用「移入回收站」。'))
    renderTrash()
    await selectFirst()
    const btn = screen.getByTestId('trash-purge-selected') as HTMLButtonElement
    expect(btn.disabled).toBe(true)
    expect(screen.getByTestId('trash-purge-note').textContent).toContain('彻底删除当前不可用')
    expect(screen.getByTestId('trash-purge-note').textContent).toContain('网页版不支持物理删除磁盘文件')
  })

  it('点「彻底删除 2 张」只弹确认框：真实张数 + 不可撤销 + copyin/reference 文件影响，且此时零请求', async () => {
    renderTrash()
    fireEvent.click(await screen.findByLabelText('选择 a11.jpg')) // copyin
    fireEvent.click(await screen.findByLabelText('选择 a12.jpg')) // reference
    const btn = screen.getByTestId('trash-purge-selected') as HTMLButtonElement
    expect(btn.disabled).toBe(false)
    fireEvent.click(btn)

    const dialog = await screen.findByTestId('confirm-dialog')
    expect(dialog.textContent).toContain('彻底删除 2 张？')
    expect(dialog.textContent).toContain('库记录与缩略图：2 张')
    // 磁盘边界分开说：copyin（库内副本，会删文件）vs reference（原地文件，只删记录）
    expect(dialog.textContent).toContain('库内副本原图：1 张（磁盘文件一并删除）')
    expect(dialog.textContent).toContain('原地文件：1 张（只删库记录，你磁盘上的原文件保留）')
    expect(dialog.textContent).toContain('不可撤销')
    expect(screen.getByTestId('confirm-ok').textContent).toBe('彻底删除 2 张')
    // 关键：确认前一个删除请求都不许发（用户要求的「先弹框」）
    expect(apiStub.dedupPurge).not.toHaveBeenCalled()
  })

  it('确认后才调 dedupPurge 并刷新列表；取消则零请求、条目不变', async () => {
    renderTrash()
    await selectFirst()
    fireEvent.click(screen.getByTestId('trash-purge-selected'))
    await screen.findByTestId('confirm-dialog')
    // 先试取消
    fireEvent.click(screen.getByTestId('confirm-cancel'))
    await waitFor(() => expect(screen.queryByTestId('confirm-dialog')).toBeNull())
    expect(apiStub.dedupPurge).not.toHaveBeenCalled()

    // 再来一次，这回确认
    fireEvent.click(screen.getByTestId('trash-purge-selected'))
    await screen.findByTestId('confirm-dialog')
    const before = apiStub.dedupTrash.mock.calls.length
    fireEvent.click(screen.getByTestId('confirm-ok'))

    await waitFor(() => expect(apiStub.dedupPurge).toHaveBeenCalledTimes(1))
    expect(apiStub.dedupPurge).toHaveBeenCalledWith([11])
    // 列表被重新拉取（refresh），且 toast 里说明释放了多少
    await waitFor(() => expect(apiStub.dedupTrash.mock.calls.length).toBeGreaterThan(before))
    await waitFor(() => expect(screen.getByText(/已彻底删除 1 张/)).toBeTruthy())
  })

  it('后端如实回 skipped 时 toast 不谎报：purged=0 → 警告「没有可彻底删除的图」', async () => {
    apiStub.dedupPurge.mockImplementation(async () => ({
      ok: true,
      purged: 0,
      skipped: 1,
      purgedIds: [],
      skippedIds: [11],
      rows: 0,
      vectors: 0,
      thumbs: 0,
      filesDeleted: 0,
      freedBytes: 0,
    }))
    renderTrash()
    await selectFirst()
    fireEvent.click(screen.getByTestId('trash-purge-selected'))
    await screen.findByTestId('confirm-dialog')
    fireEvent.click(screen.getByTestId('confirm-ok'))
    await waitFor(() => expect(screen.getByText(/没有可彻底删除的图/)).toBeTruthy())
    expect(screen.getByText(/跳过 1 张/)).toBeTruthy()
  })
})
