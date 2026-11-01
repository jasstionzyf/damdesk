// @vitest-environment jsdom
// @vitest-environment-options {"url": "http://localhost:3000/"}
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { MemoryRouter, Route, Routes } from 'react-router-dom'
import { ToastProvider } from '../src/lib/toast'
import { ConfirmProvider } from '../src/lib/confirm'
import { __setCapabilitiesForTest } from '../src/lib/caps'
import { TaskCenterProvider } from '../src/lib/tasks'
import { setDragPayload } from '../src/lib/dnd'
import type { AssetView, Capabilities } from '../src/lib/api'

/**
 * KI-10（卡 t_a60efa81）验收：**触点接线**的 DOM 断言。
 *
 * 机制层（`<PathLink>` 自身的行为）已由 `pathlink.dom.test.tsx` 钉住；本文件钉的是
 * 「页面到底有没有接上」—— 每个触点上必须出现同一个组件，且模式对：
 *  - 资产级（`mode="reveal"`）：Inspector 路径行、回收站原路径、合规侧栅成员；
 *  - 目录级（`mode="open"`）：库树库根 / 当前目录、任务中心的导出目标与导入批次源目录；
 *  - 形态门控：容器形态（`openInOS=false`）触点上**没有**打开按钮，只有复制路径 + 人话原因。
 *
 * 只把 `api.osReveal` 与少数读接口换成桩，其余用真实实现（`importOriginal`）。
 */

const stubs = vi.hoisted(() => ({
  osReveal: vi.fn(async (path: string, mode?: 'reveal' | 'open') => ({
    ok: true,
    path,
    mode: mode ?? 'reveal',
    label: mode === 'open' ? '打开文件夹' : '在文件夹中显示',
    program: 'xdg-open',
    args: [path],
  })),
  dedupTrash: vi.fn(),
  importBatches: vi.fn(),
  exportStatus: vi.fn(),
  complianceVerdicts: vi.fn(),
  complianceStatus: vi.fn(),
  complianceReview: vi.fn(),
  complianceJudgeStatus: vi.fn(),
  complianceVerdictsBatch: vi.fn(),
  // DM-B2（KI-37）：处理记录页 L1 取数走统一 job 列表（这里只关心目录触点，给空列表）
  complianceJobs: vi.fn(async () => ({ jobs: [], total: 0 })),
  assets: vi.fn(),
  capabilities: vi.fn(async () => null),
  thumbUrl: (id: number, size: 256 | 1024 = 256) => `/api/media/thumb/${id}?size=${size}`,
  originalUrl: (id: number) => `/api/media/original/${id}`,
}))

vi.mock('../src/lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../src/lib/api')>()
  return { ...actual, api: { ...actual.api, ...stubs } }
})

import { Inspector } from '../src/components/Inspector'
import { LibraryTree, chipsOf, defaultFilter } from '../src/components/LibraryTree'
import { AssetCard } from '../src/components/AssetGrid'
import { ComplianceReviewDialog } from '../src/components/ComplianceReviewDialog'
import { Trash } from '../src/pages/Trash'
import { Tasks } from '../src/pages/Tasks'

function capsWith(openInOS: boolean, reason = ''): Capabilities {
  return {
    isDesktop: openInOS,
    storeMode: openInOS ? 'local-disk' : 'server-volume',
    writableRoots: openInOS ? ['/corpus'] : [],
    roots: [{ path: '/corpus', writable: openInOS, device: 1 }],
    features: {
      moveAsset: openInOS,
      renameAsset: openInOS,
      relink: openInOS,
      openInOS,
      physicalDelete: false,
      copyInImport: true,
      scanExternalPath: openInOS,
    },
    featureReasons: openInOS ? {} : { openInOS: reason },
  } as Capabilities
}

function assetOf(id: number): AssetView {
  return {
    assetId: id,
    mediaType: 'image',
    absPath: `/corpus/2024/a${id}.jpg`,
    storeMode: 'reference',
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
    deleted: 0,
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
  } as AssetView
}

const COMPLIANCE_STATUS = {
  pack: { id: 'p', rules: 1 },
  verdictPackId: 'p',
  states: { pass: 1, fail: 0, undetermined: 0, unjudged: 1, judged: 1, total: 2 },
  queue: { total: 1, unresolved: 1, reviewed: 0 },
  feedback: { total: 0, misjudged: 0 },
}

const REVIEW_ITEM = {
  assetId: 11,
  fileName: 'a11.jpg',
  absPath: '/corpus/2024/a11.jpg',
  thumbPath: null,
  ruleId: 'r1',
  ruleName: '主图合规',
  judgementScope: 'per_asset',
  machineResult: 'pass',
  machineConfidence: 0.9,
  machineScore: 0.9,
  reason: null,
  lowConfidence: false,
  finalStatus: 'pass',
  humanReview: null,
  reviewed: false,
  ts: 1,
}

const VERDICTS = {
  packId: 'builtin',
  assetId: 11,
  state: 'pass',
  stateLabel: '合格',
  machineState: 'pass',
  machineStateLabel: '合格',
  machineConfidence: 0.97,
  assetHumanReview: null,
  items: [],
}

beforeEach(() => {
  stubs.osReveal.mockClear()
  stubs.dedupTrash.mockResolvedValue({ items: [assetOf(11)] })
  stubs.importBatches.mockResolvedValue([
    {
      batchId: 7,
      rootPath: '/corpus/incoming',
      storeMode: 'reference',
      status: 'done',
      totalFiles: 12,
      doneFiles: 12,
      error: null,
      startedAt: 1_700_000_000,
      endedAt: 1_700_000_100,
    },
  ])
  stubs.exportStatus.mockResolvedValue({
    running: false,
    progress: null,
    report: {
      dest: '/corpus/exports/export-1',
      total: 3,
      exported: 3,
      skipped: [],
      errors: [],
      cancelled: false,
      manifestWritten: true,
    },
  })
  stubs.complianceVerdicts.mockResolvedValue(VERDICTS)
  stubs.complianceStatus.mockResolvedValue(COMPLIANCE_STATUS)
  stubs.complianceReview.mockResolvedValue({
    packId: 'p',
    total: 1,
    threshold: 0.7,
    lowConfidenceOnly: false,
    items: [REVIEW_ITEM],
  })
  stubs.complianceJudgeStatus.mockResolvedValue({ running: false, cancelling: false, progress: null, report: null })
  stubs.assets.mockResolvedValue({ total: 1, items: [assetOf(11)] })
  stubs.complianceVerdictsBatch.mockResolvedValue({
    packId: 'p',
    items: [
      {
        assetId: 11,
        state: 'pass',
        stateLabel: '合格',
        machineState: 'pass',
        machineStateLabel: '合格',
        machineConfidence: 0.9,
        assetHumanReview: null,
        items: [],
      },
    ],
  })
  __setCapabilitiesForTest(capsWith(true))
})

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
  __setCapabilitiesForTest(null)
})

/** 触点上必须渲染出的「打开」按钮文案（两种语义各有唯一文案） */
async function expectRevealAt(path: string, label: '在文件夹中显示' | '打开文件夹') {
  const links = await screen.findAllByTestId('pathlink')
  expect(links.length).toBeGreaterThan(0)
  // full 形态正文就是全路径；compact 形态只有名字，全路径落在 title（两种都按 title 认）
  const hit = links.find(
    (l) => l.querySelector(`[data-testid="pathlink-path"][title="${path}"]`) !== null,
  )!
  expect(hit).toBeTruthy()
  expect(hit.getAttribute('data-mode')).toBe(label === '打开文件夹' ? 'open' : 'reveal')
  const btn = hit.querySelector('[data-testid="pathlink-open"]') as HTMLButtonElement
  expect(btn.textContent).toBe(label)
  fireEvent.click(btn)
  await waitFor(() => expect(stubs.osReveal).toHaveBeenCalledTimes(1))
  return hit
}

describe('KI-10 触点接线：资产级「在文件夹中显示」', () => {
  it('Inspector 路径行 → 可点（reveal），点击调 /api/os/reveal', async () => {
    render(
      <ToastProvider>
        <MemoryRouter>
        <Inspector
          asset={assetOf(11)}
          batch={[]}
          dirs={[]}
          busy={false}
          similarBusy={false}
          tagsVersion={0}
          actions={{} as never}
          onPreview={() => {}}
        />
        </MemoryRouter>
      </ToastProvider>,
    )
    await expectRevealAt('/corpus/2024/a11.jpg', '在文件夹中显示')
    expect(stubs.osReveal).toHaveBeenCalledWith('/corpus/2024/a11.jpg', 'reveal')
  })

  it('回收站行 → 原路径可点（reveal）', async () => {
    render(
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
    await expectRevealAt('/corpus/2024/a11.jpg', '在文件夹中显示')
  })

  it('合规改判对话框 → 改判前可打开原图所在目录（reveal）', async () => {
    render(
      <ToastProvider>
        <ComplianceReviewDialog
          target={{
            assetId: 11,
            fileName: 'a11.jpg',
            absPath: '/corpus/2024/a11.jpg',
            ruleId: null,
            ruleName: '整张图结论',
            machineResult: 'pass',
            machineConfidence: 0.97,
            threshold: 0.8,
            hasReview: false,
            currentHuman: null,
          }}
          onSubmit={async () => ({}) as never}
          onRevoke={async () => ({}) as never}
          onClose={() => {}}
          onDone={() => {}}
        />
      </ToastProvider>,
    )
    await expectRevealAt('/corpus/2024/a11.jpg', '在文件夹中显示')
  })
})

describe('KI-10 触点接线：目录级「打开文件夹」', () => {
  it('图库库树 → 每个库根都可点（open）', async () => {
    render(
      <MemoryRouter>
        <LibraryTree
          stats={null}
          dirs={[{ dir: '/corpus/2024', count: 3 }]}
          roots={['/corpus']}
          tags={[]}
          filter={defaultFilter}
          dragActive={false}
          onSelect={() => {}}
          onRefreshDirs={() => {}}
          onDropMove={() => {}}
          onDropTag={() => {}}
        />
      </MemoryRouter>,
    )
    await expectRevealAt('/corpus', '打开文件夹')
  })

  it('DT2 图库库树 → 选中目录后出现「当前目录」入口（open，compact 只显名字）', async () => {
    render(
      <MemoryRouter>
        <LibraryTree
          stats={null}
          dirs={[{ dir: '/corpus/2024', count: 3 }]}
          roots={['/corpus']}
          tags={[]}
          filter={{ ...defaultFilter, dir: '/corpus/2024' }}
          dragActive={false}
          onSelect={() => {}}
          onRefreshDirs={() => {}}
          onDropMove={() => {}}
          onDropTag={() => {}}
        />
      </MemoryRouter>,
    )
    const box = await screen.findByTestId('library-current-dir')
    const name = box.querySelector('[data-testid="pathlink-path"]') as HTMLElement
    // 侧栏 240px：主线只放 basename，全路径在 title（悬停 / 复制路径仍能拿到）
    expect(name.textContent).toBe('2024')
    expect(name.getAttribute('title')).toBe('/corpus/2024')
    const btn = box.querySelector('[data-testid="pathlink-open"]') as HTMLButtonElement
    expect(btn.textContent).toBe('打开文件夹')
    fireEvent.click(btn)
    await waitFor(() => expect(stubs.osReveal).toHaveBeenCalledWith('/corpus/2024', 'open'))
  })

  it('DT2 图库库树 → 侧栏库根走 compact：名字 + 卷/父目录副行，同列表重名自动多露一级', async () => {
    render(
      <MemoryRouter>
        <LibraryTree
          stats={null}
          dirs={[{ dir: '/corpus/2024', count: 3 }]}
          roots={['/test-fixture/lib', '/backup/lib']}
          tags={[]}
          filter={defaultFilter}
          dragActive={false}
          onSelect={() => {}}
          onRefreshDirs={() => {}}
          onDropMove={() => {}}
          onDropTag={() => {}}
        />
      </MemoryRouter>,
    )
    const box = await screen.findByTestId('library-roots')
    const links = Array.from(box.querySelectorAll('[data-testid="pathlink"]')) as HTMLElement[]
    expect(links.map((l) => l.getAttribute('data-display'))).toEqual(['compact', 'compact'])

    const names = links.map((l) => l.querySelector('[data-testid="pathlink-path"]')!.textContent)
    // 两个库根都叫 lib → 各自多露一级父目录，仍然可区分
    expect(names).toEqual(['test-fixture/lib', 'backup/lib'])
    const hints = links.map((l) => l.querySelector('[data-testid="pathlink-hint"]')!.textContent)
    expect(hints).toEqual(['/test-fixture', '/backup'])
    expect(links[0].querySelector('[data-testid="pathlink-path"]')!.getAttribute('title')).toBe('/test-fixture/lib')
  })

  it('DT2 图库库树 → 标签行 + 拖拽提示（同文件其它行不因 compact 改动失效）', async () => {
    const onSelect = vi.fn()
    render(
      <MemoryRouter>
        <LibraryTree
          stats={null}
          dirs={[]}
          roots={[]}
          tags={[{ value: '狗', count: 2 } as never]}
          filter={defaultFilter}
          dragActive
          onSelect={onSelect}
          onRefreshDirs={() => {}}
          onDropMove={() => {}}
          onDropTag={() => {}}
        />
      </MemoryRouter>,
    )
    const tagRow = await screen.findByTestId('drop-tag-狗')
    expect(tagRow.textContent).toContain('狗')
    fireEvent.click(tagRow)
    expect(onSelect).toHaveBeenCalledWith(expect.objectContaining({ tag: '狗', view: 'all' }))
    expect(screen.getByTestId('drag-hint')).toBeTruthy()
  })

  it('DT2 图库库树 → 分区行 / 目录行全部可点，目录行可拖放移动（改动不遮其它行）；KI-11 评分段已移除', async () => {
    const onSelect = vi.fn()
    const onDropMove = vi.fn()
    render(
      <MemoryRouter>
        <LibraryTree
          stats={{ assets: 10, tagged: 6, missing: 2, broken: 1 } as never}
          dirs={[{ dir: '/corpus/2024', count: 3 }]}
          roots={['/corpus']}
          tags={[{ value: '狗', count: 2 } as never]}
          filter={defaultFilter}
          dragActive
          onSelect={onSelect}
          onRefreshDirs={() => {}}
          onDropMove={onDropMove}
          onDropTag={() => {}}
        />
      </MemoryRouter>,
    )

    fireEvent.click(screen.getByText('全部图片'))
    expect(onSelect).toHaveBeenLastCalledWith({ view: 'all' })
    fireEvent.click(screen.getByText('最近导入'))
    expect(onSelect).toHaveBeenLastCalledWith({ view: 'recent' })
    fireEvent.click(screen.getByText('未打标'))
    expect(onSelect).toHaveBeenLastCalledWith(expect.objectContaining({ untagged: true }))
    fireEvent.click(screen.getByTestId('row-missing'))
    expect(onSelect).toHaveBeenLastCalledWith(expect.objectContaining({ missing: 1 }))
    fireEvent.click(screen.getByTestId('row-broken'))
    expect(onSelect).toHaveBeenLastCalledWith(expect.objectContaining({ broken: 1 }))
    // DECouple-FE（卡 t_ad156948）：图库侧栏不再有「合规」判定分桶（判定只在任务中心）
    expect(screen.queryByTestId('compliance-section')).toBeNull()
    expect(screen.queryByTestId('row-compliance-fail')).toBeNull()
    // KI-11：侧栏不再有「评分」段与星级行
    expect(screen.queryByText('评分')).toBeNull()
    expect(screen.queryByText('4 星及以上')).toBeNull()

    // 目录行：点击 = 筛选该目录；「查重」按钮 = 带范围跳 /dedup
    const dirRow = screen.getByTestId('drop-dir-/corpus/2024')
    fireEvent.click(dirRow)
    expect(onSelect).toHaveBeenLastCalledWith(expect.objectContaining({ dir: '/corpus/2024' }))
    expect(screen.getByTestId('dedup-dir-/corpus/2024')).toBeTruthy()

    // 拖放：dragLeave 在未悬停时直接返回；dragOver 高亮；drop 上报 ids→移动
    fireEvent.dragLeave(dirRow)
    setDragPayload([11, 12])
    fireEvent.dragOver(dirRow, { dataTransfer: { dropEffect: '' } })
    fireEvent.drop(dirRow, { dataTransfer: {} })
    await waitFor(() => expect(onDropMove).toHaveBeenCalledWith([11, 12], '/corpus/2024'))

    // 非拖放目标行（失联）dragOver 不接管
    fireEvent.dragOver(screen.getByTestId('row-missing'), { dataTransfer: { dropEffect: '' } })
  })

  it('DT2 chipsOf：EXIF 维度芯片（画幅 / 相机 / 拍摄区间）', () => {
    const chips = chipsOf({
      ...defaultFilter,
      aspect: 'portrait',
      cameraModel: 'X100V',
      takenFrom: 1_700_000_000,
    } as never)
    const labels = chips.map((c) => c.label)
    expect(labels).toEqual(expect.arrayContaining(['竖图', '相机 X100V']))
    expect(chips.find((c) => c.field === 'takenRange')!.label).toContain('…')
    expect(chipsOf({ ...defaultFilter, aspect: 'landscape' } as never)[0].label).toBe('横图')
    expect(chipsOf({ ...defaultFilter, aspect: 'square' } as never)[0].label).toBe('方图')
  })

  it('任务中心 → 导出目标目录可点（open）+ 导入批次源目录可点（open）', async () => {
    render(
      <MemoryRouter>
        <ToastProvider>
          <TaskCenterProvider>
            <Tasks />
          </TaskCenterProvider>
        </ToastProvider>
      </MemoryRouter>,
    )
    await waitFor(() => expect(stubs.exportStatus).toHaveBeenCalled())
    // DM-B2（KI-37）：导出/导入各是统一 job 列表里的一行，点开才看那一次的明细
    fireEvent.click(await screen.findByTestId('job-row-export:last'))
    const exportBox = await screen.findByTestId('export-report')
    const exportBtn = exportBox.querySelector('[data-testid="pathlink-open"]') as HTMLButtonElement
    expect(exportBtn.textContent).toBe('打开文件夹')
    fireEvent.click(exportBtn)
    await waitFor(() => expect(stubs.osReveal).toHaveBeenCalledWith('/corpus/exports/export-1', 'open'))

    fireEvent.click(await screen.findByTestId('job-row-import:7'))
    const batch = await screen.findByTestId('job-import-detail')
    expect(batch.textContent).toContain('/corpus/incoming')
    const batchBtn = batch.querySelector('[data-testid="pathlink-open"]') as HTMLButtonElement
    expect(batchBtn.textContent).toBe('打开文件夹')
    fireEvent.click(batchBtn)
    await waitFor(() => expect(stubs.osReveal).toHaveBeenCalledWith('/corpus/incoming', 'open'))
  })
})

describe('KI-10 触点接线：图库卡片右键 + 容器形态门控', () => {
  it('图库卡片右键 → 把该资产的路径交给菜单（卡片接线，AssetGrid 原样透传）', async () => {
    const onContextMenu = vi.fn()
    render(
      <AssetCard
        asset={assetOf(11)}
        isSel={false}
        isActive={false}
        selectedIds={[]}
        onPick={() => {}}
        onOpen={() => {}}
        onContextMenu={onContextMenu}
      />,
    )
    const card = await screen.findByTestId('asset-card-11')
    fireEvent.contextMenu(card, { clientX: 40, clientY: 60 })
    await waitFor(() => expect(onContextMenu).toHaveBeenCalledTimes(1))
    expect(onContextMenu.mock.calls[0][0].absPath).toBe('/corpus/2024/a11.jpg')
    // 右键必须被接管（否则弹出浏览器原生菜单，用户看不到「在文件夹中显示」）
    const ev = new MouseEvent('contextmenu', { bubbles: true, cancelable: true })
    card.dispatchEvent(ev)
    expect(ev.defaultPrevented).toBe(true)
  })

  it('容器形态（openInOS=false）→ 触点上没有打开按钮，只有复制路径 + 人话原因', async () => {
    __setCapabilitiesForTest(capsWith(false, '网页版无法在系统文件管理器中打开——文件在服务器上。'))
    render(
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
    const link = (await screen.findAllByTestId('pathlink')).find((l) =>
      l.textContent?.includes('/corpus/2024/a11.jpg'),
    )!
    expect(link).toBeTruthy()
    expect(link.getAttribute('data-gate')).toBe('copy')
    expect(link.querySelector('[data-testid="pathlink-open"]')).toBeNull()
    expect(link.querySelector('[data-testid="pathlink-copy"]')).not.toBeNull()
    expect(link.querySelector('[data-testid="pathlink-reason"]')?.textContent).toContain(
      '网页版无法在系统文件管理器中打开',
    )
    expect(stubs.osReveal).not.toHaveBeenCalled()
  })
})
