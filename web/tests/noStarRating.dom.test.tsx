// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest'
import { cleanup, render, screen } from '@testing-library/react'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { ToastProvider } from '../src/lib/toast'
import { Lightbox } from '../src/components/Lightbox'
import { AssetCard } from '../src/components/AssetGrid'
import type { AssetView } from '../src/lib/api'

/**
 * KI-11（卡 task）：五档星级评分从**界面**全部移除 —— 质量分级用定制标签表达。
 *
 * 两层断言：
 *  1. 静态棘轮（源码读取）：7 处 UI 落点（侧栏/图库/搜索/检查器/Lightbox/卡片/导出/智能文件夹/
 *     快捷键面板）不允许再出现 ★/☆ 字形、starRating/minRating 引用、打分 testid。
 *     数据层（api.ts 类型、api.rate 方法、searchResults 回读默认值）保留，不在本棘轮范围内。
 *  2. DOM 断言：Lightbox（大图 + 翻页）、AssetCard（含 dirty 角标）、Inspector（单选 + 批量）
 *     渲染后无任何星级字形 / 「评分」「打分」字样、无打分 testid；AssetCard 的
 *     「已修改」角标（KI-12）正常出现。
 */

const SRC = resolve(__dirname, '../src')

/**
 * KI-11 的 7 处 UI 落点 + 快捷键面板（键位提示也是界面文案）。
 * 卡 task 撤销 pages/ExportPage.tsx（导出收编为全局 Dialog）→ 该落点由
 * components/ExportDialog.tsx 继续代表「导出」这一处，文件数减 1。
 */
const UI_FILES = [
  'components/LibraryTree.tsx',
  'components/Inspector.tsx',
  'components/Lightbox.tsx',
  'components/AssetGrid.tsx',
  'components/SmartFolderDialog.tsx',
  'components/ShortcutsHelp.tsx',
  'pages/Gallery.tsx',
  'pages/Search.tsx',
  'components/ExportDialog.tsx',
  'pages/SmartFolderRedirect.tsx',
] as const

/** 界面不允许再出现的星级痕迹 */
const FORBIDDEN = [/★/, /☆/, /starRating/, /minRating/, /lightbox-star-/, /batch-rate-/, /insp-rate-/, /asset-stars-/]

describe('KI-11 静态棘轮：7 处 UI 落点无星级痕迹', () => {
  for (const f of UI_FILES) {
    it(`${f} 无 ★/☆、无 starRating/minRating、无打分 testid`, () => {
      const src = readFileSync(resolve(SRC, f), 'utf-8')
      for (const re of FORBIDDEN) {
        expect({ file: f, rule: String(re), hit: re.test(src) }).toEqual({ file: f, rule: String(re), hit: false })
      }
    })
  }
})

function assetOf(id: number): AssetView {
  return {
    assetId: id,
    mediaType: 'image',
    absPath: `/library/snow-${id}.jpg`,
    storeMode: 'index',
    missing: 0,
    fileName: `snow-${id}.jpg`,
    fileExt: 'jpg',
    fileSz: 2048,
    mtime: 0,
    fileHash: null,
    titleCh: '雪山日出',
    titleEn: null,
    description: null,
    source: 'index',
    parentAssetId: null,
    searchable: 1,
    deleted: 0,
    approvalStatus: 'pending',
    starRating: 3, // 旧库可能带着历史打星：界面必须无视它（列保留，无入口）
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
  thumbUrl: (id: number, size: 256 | 1024 = 256) => `/api/media/thumb/${id}?size=${size}`,
  originalUrl: (id: number) => `/api/media/original/${id}`,
  capabilities: vi.fn(),
  assetTags: vi.fn(),
  ocrAssetText: vi.fn(),
  assetTagResults: vi.fn(),
  complianceVerdicts: vi.fn(),
  complianceReview: vi.fn(),
  complianceReviewCreate: vi.fn(),
  complianceReviewRevoke: vi.fn(),
}))

vi.mock('../src/lib/api', () => ({
  api: apiStub,
  formatSize: (n: number) => `${n} B`,
  ApiError: class ApiError extends Error {
    code?: string
    status = 500
  },
}))

apiStub.capabilities.mockImplementation(async () => ({ features: {} }))
apiStub.assetTags.mockImplementation(async () => [])
apiStub.ocrAssetText.mockImplementation(async (assetId: number) => ({ assetId, status: 'none', text: null, blocks: [] }))
apiStub.assetTagResults.mockImplementation(async () => [])
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
apiStub.complianceReview.mockImplementation(async () => ({ items: [], total: 0, nextCursor: null }))

describe('KI-11 DOM 断言：渲染层无星级', () => {
  afterEach(() => cleanup())

  it('Lightbox：只有大图 + 翻页（无星级行、无拒选按钮、无打分键位提示）', () => {
    render(
      <ToastProvider>
        <Lightbox items={[assetOf(1), assetOf(2)]} index={0} onIndex={() => {}} onClose={() => {}} onHelp={() => {}} />
      </ToastProvider>,
    )
    expect(screen.getByTestId('lightbox')).toBeTruthy()
    expect(screen.getByTestId('lightbox-image')).toBeTruthy()
    expect(screen.getByTestId('lightbox-next')).toBeTruthy()
    const html = document.body.textContent ?? ''
    expect(html).not.toContain('★')
    expect(html).not.toContain('☆')
    expect(html).not.toContain('评分')
    expect(html).not.toContain('打分')
    expect(html).not.toContain('拒选 (X)')
    expect(screen.queryByTestId('lightbox-reject')).toBeNull()
  })

  it('AssetCard：旧数据带 starRating 也不显示星级；dirty 时显示「已修改」角标', () => {
    render(
      <ToastProvider>
        <AssetCard asset={assetOf(7)} isSel={false} isActive={false} selectedIds={[]} onPick={() => {}} onOpen={() => {}} dirty />
      </ToastProvider>,
    )
    const html = document.body.textContent ?? ''
    expect(html).not.toContain('★')
    expect(html).not.toContain('☆')
    expect(screen.queryByTestId('asset-stars-7')).toBeNull()
    // KI-12：结果集不重算的「已修改」角标在
    expect(screen.getByTestId('asset-dirty-7').textContent).toBe('已修改')
  })

  it('Inspector 单选：无评分区块（属性/标签/色标/合规照常）', async () => {
    const { Inspector } = await import('../src/components/Inspector')
    render(
      <ToastProvider>
        <Inspector
          asset={assetOf(9)}
          batch={[assetOf(9)]}
          dirs={[]}
          busy={false}
          similarBusy={false}
          tagsVersion={0}
          onPreview={() => {}}
          actions={{
            reject: () => {},
            addTags: () => {},
            removeTag: () => {},
            setColor: () => {},
            moveTo: () => {},
            findSimilar: () => {},
          }}
        />
      </ToastProvider>,
    )
    expect(screen.getByTestId('insp-colors')).toBeTruthy()
    const html = document.body.textContent ?? ''
    expect(html).not.toContain('评分')
    expect(html).not.toContain('★')
    expect(screen.queryByTestId('insp-rate-1')).toBeNull()
  })

  it('Inspector 批量：无评分区块（拒选/打标/移动照常）', async () => {
    const { Inspector } = await import('../src/components/Inspector')
    const batch = [assetOf(11), assetOf(12)]
    render(
      <ToastProvider>
        <Inspector
          asset={batch[0]}
          batch={batch}
          dirs={[]}
          busy={false}
          similarBusy={false}
          tagsVersion={0}
          onPreview={() => {}}
          actions={{
            reject: () => {},
            addTags: () => {},
            removeTag: () => {},
            setColor: () => {},
            moveTo: () => {},
            findSimilar: () => {},
          }}
        />
      </ToastProvider>,
    )
    expect(screen.getByTestId('batch-reject')).toBeTruthy()
    const html = document.body.textContent ?? ''
    expect(html).not.toContain('评分')
    expect(html).not.toContain('★')
    expect(screen.queryByTestId('batch-rate-3')).toBeNull()
    expect(screen.queryByTestId('batch-rate-clear')).toBeNull()
  })
})
